//! A deterministic simulator: run a whole plan to its end with a stubbed
//! executor and no live agents at all.
//!
//! This exists because the scheduler and the mutation rules are the part of a
//! multi-agent system that is hardest to debug once real workers are attached: a
//! stuck plan looks exactly like a slow one, and a wrongly-ordered dispatch
//! looks exactly like a model being unhelpful. Running the graph against a
//! scripted executor separates the two — if a plan misbehaves here, it is the
//! engine; if it only misbehaves live, it is not.
//!
//! Determinism is the whole point, so there is no randomness, no clock, and no
//! task scheduling: each round takes the ready frontier in id order, dispatches
//! up to the concurrency budget, and resolves those in dispatch order. Real
//! workers finish out of order, but *what* they may do — and in what order the
//! graph offers it — is exactly what this pins.

use crate::error::PlanError;
use crate::ops::{
    complete_node, expand_node, fail_node, inject_from_gate, salvage_assignment, Salvage,
};
use crate::plan::{ActorId, NodeId, NodeSpec, TaskPlan};
use crate::schedule::{cascade_blocked, dispatch, progress, ready_nodes, Assignment, PlanProgress};

use crate::artifact::HandoffArtifact;

/// What a simulated worker does with an assignment.
#[derive(Debug, Clone, PartialEq)]
pub enum SimAction {
    /// Finish it with this handoff.
    Complete(Box<HandoffArtifact>),
    /// Decide it is too big and break it up.
    Expand(Vec<NodeSpec>),
    /// A gate raising findings.
    Inject(Vec<NodeSpec>),
    /// Finish it badly.
    Fail(String),
    /// Stop reporting entirely — the worker died holding the task. Exercises the
    /// salvage path without a process to kill.
    Vanish,
}

/// A stand-in for a worker.
pub trait SimExecutor {
    /// Decide what this assignment does. Called once per dispatch, in dispatch
    /// order, so an implementation may keep counters and stay deterministic.
    fn execute(&mut self, assignment: &Assignment, plan: &TaskPlan) -> SimAction;
}

/// Any suitable closure is an executor.
impl<F> SimExecutor for F
where
    F: FnMut(&Assignment, &TaskPlan) -> SimAction,
{
    fn execute(&mut self, assignment: &Assignment, plan: &TaskPlan) -> SimAction {
        self(assignment, plan)
    }
}

/// How the simulation is bounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimConfig {
    /// How many tasks may be in flight at once.
    pub concurrency: usize,
    /// A hard ceiling on rounds. A plan that hits it is a bug in the plan or the
    /// engine, and stopping loudly beats looping forever in a test run.
    pub max_rounds: usize,
    /// How many times a task may be salvaged before it fails.
    pub reclaim_limit: u32,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            concurrency: 4,
            max_rounds: 256,
            reclaim_limit: 2,
        }
    }
}

/// One thing that happened, in the order it happened.
#[derive(Debug, Clone, PartialEq)]
pub enum SimEvent {
    /// A task was handed to a worker.
    Dispatched {
        /// Which round.
        round: usize,
        /// The task.
        node: NodeId,
        /// Who took it.
        assignee: ActorId,
    },
    /// A task finished well.
    Completed(NodeId),
    /// A task was broken up; the children are the new ids.
    Expanded {
        /// The task that decomposed.
        node: NodeId,
        /// What it became.
        children: Vec<NodeId>,
    },
    /// A gate raised findings and added work.
    Injected {
        /// The gate.
        node: NodeId,
        /// The remediation tasks.
        added: Vec<NodeId>,
    },
    /// A task finished badly.
    Failed(NodeId),
    /// A worker stopped reporting and its task was taken back.
    Salvaged {
        /// The task.
        node: NodeId,
        /// What the salvage decided.
        outcome: Salvage,
    },
    /// A task was dropped because something upstream never completed.
    Abandoned(NodeId),
    /// A mutation was refused. Recorded rather than swallowed: a simulation
    /// where the engine kept saying no is a *finding*, not a clean run.
    Refused {
        /// The task the refused mutation targeted.
        node: NodeId,
        /// Why.
        error: PlanError,
    },
}

/// What a simulation did.
#[derive(Debug, Clone, PartialEq)]
pub struct SimReport {
    /// Everything that happened, in order.
    pub events: Vec<SimEvent>,
    /// How many rounds it took.
    pub rounds: usize,
    /// Whether every node reached a terminal state.
    pub settled: bool,
    /// Whether the round ceiling was hit.
    pub exhausted: bool,
    /// Where the plan ended up.
    pub progress: PlanProgress,
}

impl SimReport {
    /// The tasks that were dispatched, in dispatch order. The assertion most
    /// tests actually want.
    #[must_use]
    pub fn dispatch_order(&self) -> Vec<NodeId> {
        self.events
            .iter()
            .filter_map(|event| match event {
                SimEvent::Dispatched { node, .. } => Some(*node),
                _ => None,
            })
            .collect()
    }

    /// Whether the engine refused anything.
    #[must_use]
    pub fn refusals(&self) -> Vec<&PlanError> {
        self.events
            .iter()
            .filter_map(|event| match event {
                SimEvent::Refused { error, .. } => Some(error),
                _ => None,
            })
            .collect()
    }
}

/// Run `plan` to completion against `executor`.
///
/// Mutates the plan in place, so the caller can inspect the final graph as well
/// as the report.
pub fn simulate(
    plan: &mut TaskPlan,
    executor: &mut impl SimExecutor,
    config: SimConfig,
) -> SimReport {
    let mut events = Vec::new();
    let mut round = 0;
    let concurrency = config.concurrency.max(1);

    while round < config.max_rounds {
        // A blocked tail is settled before the frontier is read, so a plan that
        // is over stops looking like a plan that is waiting.
        for id in cascade_blocked(plan) {
            events.push(SimEvent::Abandoned(id));
        }
        let frontier: Vec<NodeId> = ready_nodes(plan).into_iter().take(concurrency).collect();
        if frontier.is_empty() {
            break;
        }
        round += 1;

        let mut assignments = Vec::new();
        for (slot, node) in frontier.iter().enumerate() {
            let assignee = ActorId::new(format!("worker-{slot}"));
            match dispatch(plan, *node, &assignee) {
                Ok(assignment) => {
                    events.push(SimEvent::Dispatched {
                        round,
                        node: *node,
                        assignee,
                    });
                    assignments.push(assignment);
                }
                Err(error) => events.push(SimEvent::Refused { node: *node, error }),
            }
        }

        for assignment in assignments {
            let action = executor.execute(&assignment, plan);
            apply(plan, &assignment, action, config.reclaim_limit, &mut events);
        }
    }

    for id in cascade_blocked(plan) {
        events.push(SimEvent::Abandoned(id));
    }
    SimReport {
        events,
        rounds: round,
        settled: plan.is_settled(),
        exhausted: round >= config.max_rounds,
        progress: progress(plan),
    }
}

fn apply(
    plan: &mut TaskPlan,
    assignment: &Assignment,
    action: SimAction,
    reclaim_limit: u32,
    events: &mut Vec<SimEvent>,
) {
    let node = assignment.node;
    let actor = &assignment.assignee;
    match action {
        SimAction::Complete(artifact) => {
            match complete_node(plan, node, actor, *artifact) {
                Ok(()) => events.push(SimEvent::Completed(node)),
                Err(error) => events.push(SimEvent::Refused { node, error }),
            };
        }
        SimAction::Expand(specs) => match expand_node(plan, node, actor, &specs) {
            Ok(expanded) => events.push(SimEvent::Expanded {
                node,
                children: expanded.children,
            }),
            Err(error) => events.push(SimEvent::Refused { node, error }),
        },
        SimAction::Inject(specs) => match inject_from_gate(plan, node, actor, &specs) {
            Ok(added) => events.push(SimEvent::Injected { node, added }),
            Err(error) => events.push(SimEvent::Refused { node, error }),
        },
        SimAction::Fail(reason) => match fail_node(plan, node, actor, reason) {
            Ok(()) => events.push(SimEvent::Failed(node)),
            Err(error) => events.push(SimEvent::Refused { node, error }),
        },
        SimAction::Vanish => match salvage_assignment(plan, node, reclaim_limit) {
            Ok(outcome) => events.push(SimEvent::Salvaged { node, outcome }),
            Err(error) => events.push(SimEvent::Refused { node, error }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::Confidence;
    use crate::ops::seed;
    use crate::plan::PlanMode;

    fn lead() -> ActorId {
        ActorId::new("lead")
    }

    /// A handoff good enough for any node, including a review gate — so a test
    /// that is about scheduling does not fail on artifact validation.
    fn handoff(title: &str) -> SimAction {
        SimAction::Complete(Box::new(
            HandoffArtifact::new(format!("{title} is done"), Confidence::new(0.8))
                .with_gap("nothing beyond the task")
                .with_validation("re-ran the checks the task named")
                .with_evidence("simulated"),
        ))
    }

    fn fan_out(mode: PlanMode) -> TaskPlan {
        let mut plan = TaskPlan::new("ship it", mode, lead());
        let specs = vec![
            NodeSpec::task("survey", "look around"),
            NodeSpec::task("left", "do the left half").after(0),
            NodeSpec::task("right", "do the right half").after(0),
            NodeSpec::task("join", "put them together")
                .after(1)
                .after(2),
        ];
        seed(&mut plan, &lead(), "k1", &specs).unwrap();
        plan
    }

    #[test]
    fn a_fan_out_runs_to_completion_in_dependency_order() {
        let mut plan = fan_out(PlanMode::Light);
        let report = simulate(
            &mut plan,
            &mut |a: &Assignment, _: &TaskPlan| handoff(&a.title),
            SimConfig::default(),
        );
        assert!(report.settled, "{:?}", report.progress);
        assert!(report.refusals().is_empty(), "{:?}", report.refusals());
        let order = report.dispatch_order();
        assert_eq!(order.len(), 4);
        assert_eq!(order[0].get(), 1, "the survey has to go first");
        assert_eq!(order[3].get(), 4, "the join has to go last");
    }

    #[test]
    fn the_same_plan_simulates_identically_twice() {
        let run = || {
            let mut plan = fan_out(PlanMode::Deep);
            let report = simulate(
                &mut plan,
                &mut |a: &Assignment, _: &TaskPlan| handoff(&a.title),
                SimConfig::default(),
            );
            (report, plan)
        };
        let (first, first_plan) = run();
        let (second, second_plan) = run();
        assert_eq!(first, second);
        assert_eq!(first_plan, second_plan);
    }

    #[test]
    fn concurrency_bounds_how_many_start_in_one_round() {
        let mut plan = TaskPlan::new("ship it", PlanMode::Light, lead());
        let specs: Vec<NodeSpec> = (0..5)
            .map(|i| NodeSpec::task(format!("t{i}"), "work"))
            .collect();
        seed(&mut plan, &lead(), "k1", &specs).unwrap();
        let report = simulate(
            &mut plan,
            &mut |a: &Assignment, _: &TaskPlan| handoff(&a.title),
            SimConfig {
                concurrency: 2,
                ..SimConfig::default()
            },
        );
        assert!(report.settled);
        let first_round: Vec<&SimEvent> = report
            .events
            .iter()
            .filter(|e| matches!(e, SimEvent::Dispatched { round: 1, .. }))
            .collect();
        assert_eq!(first_round.len(), 2);
        assert_eq!(report.rounds, 3);
    }

    #[test]
    fn an_expansion_is_scheduled_and_the_join_reruns() {
        let mut plan = fan_out(PlanMode::Light);
        let mut expanded_once = false;
        let report = simulate(
            &mut plan,
            &mut |a: &Assignment, _: &TaskPlan| {
                if a.title == "left" && !expanded_once {
                    expanded_once = true;
                    return SimAction::Expand(vec![
                        NodeSpec::task("left-a", "half of the left half"),
                        NodeSpec::task("left-b", "the other half"),
                    ]);
                }
                handoff(&a.title)
            },
            SimConfig::default(),
        );
        assert!(report.settled, "{:?}", report.progress);
        assert!(report.refusals().is_empty(), "{:?}", report.refusals());
        assert_eq!(plan.len(), 6);
        let order = report.dispatch_order();
        // The expanded node is dispatched twice: once when it decomposed, once
        // as the join over its children.
        assert_eq!(order.iter().filter(|id| id.get() == 2).count(), 2);
    }

    #[test]
    fn a_gate_that_injects_reruns_after_the_remediation() {
        let mut plan = fan_out(PlanMode::Deep);
        let mut injected_once = false;
        let report = simulate(
            &mut plan,
            &mut |a: &Assignment, _: &TaskPlan| {
                if a.kind == crate::plan::NodeKind::Gate && !injected_once {
                    injected_once = true;
                    return SimAction::Inject(vec![NodeSpec::task(
                        "remediate",
                        "fix what the gate found",
                    )]);
                }
                handoff(&a.title)
            },
            SimConfig::default(),
        );
        assert!(injected_once);
        assert!(report.settled, "{:?}", report.progress);
        assert!(report.refusals().is_empty(), "{:?}", report.refusals());
        let gate_dispatches = report
            .events
            .iter()
            .filter(|e| matches!(e, SimEvent::Dispatched { node, .. } if node.get() == 5))
            .count();
        assert_eq!(gate_dispatches, 2, "the gate re-reviews after remediation");
    }

    #[test]
    fn a_vanished_worker_has_its_task_taken_back_and_finished_by_another() {
        let mut plan = fan_out(PlanMode::Light);
        let mut vanished = false;
        let report = simulate(
            &mut plan,
            &mut |a: &Assignment, _: &TaskPlan| {
                if a.title == "survey" && !vanished {
                    vanished = true;
                    return SimAction::Vanish;
                }
                handoff(&a.title)
            },
            SimConfig::default(),
        );
        assert!(report.settled);
        assert!(report.events.iter().any(|e| matches!(
            e,
            SimEvent::Salvaged {
                outcome: Salvage::Requeued { reclaims: 1 },
                ..
            }
        )));
    }

    #[test]
    fn a_task_that_keeps_losing_its_worker_fails_and_strands_its_tail() {
        let mut plan = fan_out(PlanMode::Light);
        let report = simulate(
            &mut plan,
            &mut |a: &Assignment, _: &TaskPlan| {
                if a.title == "survey" {
                    SimAction::Vanish
                } else {
                    handoff(&a.title)
                }
            },
            SimConfig {
                reclaim_limit: 2,
                ..SimConfig::default()
            },
        );
        assert!(report.settled, "the plan still has to end");
        assert_eq!(report.progress.failed, 1);
        assert_eq!(report.progress.abandoned, 3);
        assert!(!report.exhausted);
    }

    #[test]
    fn a_failure_settles_the_plan_rather_than_hanging_it() {
        let mut plan = fan_out(PlanMode::Light);
        let report = simulate(
            &mut plan,
            &mut |a: &Assignment, _: &TaskPlan| {
                if a.title == "left" {
                    SimAction::Fail("the left half does not build".into())
                } else {
                    handoff(&a.title)
                }
            },
            SimConfig::default(),
        );
        assert!(report.settled);
        assert_eq!(report.progress.failed, 1);
        assert_eq!(report.progress.complete, 2);
        assert_eq!(report.progress.abandoned, 1, "the join was stranded");
    }

    #[test]
    fn a_refused_completion_is_recorded_not_swallowed() {
        let mut plan = fan_out(PlanMode::Deep);
        let report = simulate(
            &mut plan,
            // Deep mode requires the coverage gap; this omits it every time.
            &mut |_: &Assignment, _: &TaskPlan| {
                SimAction::Complete(Box::new(HandoffArtifact::new("done", Confidence::FULL)))
            },
            SimConfig::default(),
        );
        assert!(matches!(
            report.refusals().as_slice(),
            [PlanError::IncompleteArtifact { .. }]
        ));
        // A refused completion leaves the task in flight and the frontier empty:
        // the run stops rather than spinning, and reports itself unsettled
        // instead of claiming a plan that never progressed was fine.
        assert!(!report.settled);
        assert!(
            !report.exhausted,
            "it stopped because it stalled, not because it ran out of rounds"
        );
        assert_eq!(report.progress.assigned, 1);
    }
}
