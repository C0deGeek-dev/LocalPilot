//! Deciding what can run, handing it to someone, and building what they read.
//!
//! Readiness is *derived*, never stored: a task is ready when it is pending and
//! everything it waits on has completed. Storing it would create a second source
//! of truth that could disagree with the edges, and the whole value of a task
//! graph is that the edges are the truth.
//!
//! The third state matters as much as the first two. A task whose upstream
//! *failed* is neither ready nor waiting — it can never start. Without a name
//! for that, a driver waits forever on a plan that is already over, which is the
//! quiet way a scheduler hangs. [`Readiness::Blocked`] names it and
//! [`cascade_blocked`] resolves it.

use crate::artifact::HandoffArtifact;
use crate::error::PlanError;
use crate::plan::{ActorId, NodeId, NodeKind, TaskPlan, TaskStatus};

/// Whether a task can start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    /// Pending, with everything it waits on complete.
    Ready,
    /// Pending, but something upstream is still running.
    WaitingOn(Vec<NodeId>),
    /// Pending, and something upstream ended without completing — it can never
    /// start.
    Blocked(Vec<NodeId>),
    /// Not pending: in flight, or finished.
    NotPending,
}

/// Where one task stands.
///
/// # Errors
/// [`PlanError::UnknownNode`] if `node` is not in the plan.
pub fn readiness(plan: &TaskPlan, node: NodeId) -> Result<Readiness, PlanError> {
    let target = plan.node(node).ok_or(PlanError::UnknownNode(node))?;
    if target.status != TaskStatus::Pending {
        return Ok(Readiness::NotPending);
    }
    let mut waiting = Vec::new();
    let mut blocked = Vec::new();
    for up in &target.upstream {
        match plan.node(*up).map(|n| &n.status) {
            Some(TaskStatus::Complete) => {}
            // An edge to a node that is not in the plan cannot resolve; treat it
            // as blocking rather than silently ready, so a corrupted snapshot
            // surfaces as a stuck task instead of a task that ran without input.
            None => blocked.push(*up),
            Some(status) if status.is_terminal() => blocked.push(*up),
            Some(_) => waiting.push(*up),
        }
    }
    if !blocked.is_empty() {
        return Ok(Readiness::Blocked(blocked));
    }
    if !waiting.is_empty() {
        return Ok(Readiness::WaitingOn(waiting));
    }
    Ok(Readiness::Ready)
}

/// Every task that can start now, in id order.
///
/// The order is not a priority — it is a *guarantee*: the same plan produces the
/// same frontier on every machine, which is what makes the simulator worth
/// trusting.
#[must_use]
pub fn ready_nodes(plan: &TaskPlan) -> Vec<NodeId> {
    plan.nodes()
        .filter(|node| matches!(readiness(plan, node.id), Ok(Readiness::Ready)))
        .map(|node| node.id)
        .collect()
}

/// Every task that can never start because something upstream ended badly.
#[must_use]
pub fn blocked_nodes(plan: &TaskPlan) -> Vec<NodeId> {
    plan.nodes()
        .filter(|node| matches!(readiness(plan, node.id), Ok(Readiness::Blocked(_))))
        .map(|node| node.id)
        .collect()
}

/// Abandon everything that can never start, and keep going until the wave stops
/// — abandoning a task blocks *its* dependents, so one failure can strand a
/// whole tail.
///
/// This is a supervisor operation, not an actor's: it takes no owner, because
/// the actor that would have owned these tasks is precisely the one that is not
/// coming. Returns what it abandoned, in the order it did.
pub fn cascade_blocked(plan: &mut TaskPlan) -> Vec<NodeId> {
    let mut abandoned = Vec::new();
    loop {
        let wave = blocked_nodes(plan);
        if wave.is_empty() {
            return abandoned;
        }
        for id in wave {
            let reason = match readiness(plan, id) {
                Ok(Readiness::Blocked(by)) => format!(
                    "cannot start: {} did not complete",
                    by.iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                _ => continue,
            };
            if let Some(node) = plan.node_mut(id) {
                node.status = TaskStatus::Abandoned { reason };
            }
            abandoned.push(id);
        }
        plan.bump_version();
    }
}

/// Everything a worker needs to do one task.
///
/// A worker does not inherit the coordinator's context, so this has to carry it:
/// what the plan is for, what this task is, and what everything upstream already
/// established. The last part is the one that pays for the graph — a worker that
/// re-reads what its upstream already read is a worker that cost more than doing
/// the work serially.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    /// The task.
    pub node: NodeId,
    /// Its short name.
    pub title: String,
    /// Whether it is ordinary work or a review.
    pub kind: NodeKind,
    /// Who it was handed to.
    pub assignee: ActorId,
    /// The plan version this assignment was cut from, so a late report against a
    /// plan that has since changed is visible rather than silently applied.
    pub plan_version: u64,
    /// The full text handed to the worker.
    pub input: String,
}

/// Hand a ready task to an actor.
///
/// # Errors
/// [`PlanError::UnknownNode`], [`PlanError::AlreadyTerminal`],
/// [`PlanError::AlreadyAssigned`], [`PlanError::Blocked`], or
/// [`PlanError::NotReady`].
pub fn dispatch(
    plan: &mut TaskPlan,
    node: NodeId,
    assignee: &ActorId,
) -> Result<Assignment, PlanError> {
    match readiness(plan, node)? {
        Readiness::Ready => {}
        Readiness::WaitingOn(waiting_on) => return Err(PlanError::NotReady { node, waiting_on }),
        Readiness::Blocked(blocked_by) => return Err(PlanError::Blocked { node, blocked_by }),
        Readiness::NotPending => {
            let target = plan.node(node).ok_or(PlanError::UnknownNode(node))?;
            return match target.assignee() {
                Some(holder) => Err(PlanError::AlreadyAssigned {
                    node,
                    assignee: holder.clone(),
                }),
                None => Err(PlanError::AlreadyTerminal {
                    node,
                    state: target.status.label(),
                }),
            };
        }
    }

    let input = assemble_input(plan, node)?;
    let (title, kind) = plan
        .node(node)
        .map(|n| (n.title.clone(), n.kind))
        .ok_or(PlanError::UnknownNode(node))?;
    if let Some(target) = plan.node_mut(node) {
        target.status = TaskStatus::Assigned {
            assignee: assignee.clone(),
        };
    }
    plan.bump_version();
    Ok(Assignment {
        node,
        title,
        kind,
        assignee: assignee.clone(),
        plan_version: plan.version(),
        input,
    })
}

/// Build the text for one task: the objective, the task, and every upstream
/// handoff, in id order.
///
/// # Errors
/// [`PlanError::UnknownNode`] if `node` is not in the plan.
pub fn assemble_input(plan: &TaskPlan, node: NodeId) -> Result<String, PlanError> {
    let target = plan.node(node).ok_or(PlanError::UnknownNode(node))?;
    let mut out = format!(
        "Objective: {}\n\nYour task ({}): {}\n\n{}\n",
        plan.objective(),
        target.id,
        target.title,
        target.prompt.trim()
    );

    let upstream: Vec<(&NodeId, &HandoffArtifact)> = target
        .upstream
        .iter()
        .filter_map(|id| {
            plan.node(*id)
                .and_then(|n| n.artifact.as_ref().map(|a| (id, a)))
        })
        .collect();
    if upstream.is_empty() {
        return Ok(out);
    }

    out.push_str(
        "\n## What earlier tasks established\nRead this instead of redoing it. If something \
         here is wrong, say so rather than working around it.\n\n",
    );
    for (id, artifact) in upstream {
        let label = plan
            .node(*id)
            .map_or_else(|| id.to_string(), |n| format!("{} {}", n.id, n.title));
        out.push_str(&artifact.render(&label));
        out.push('\n');
    }
    Ok(out)
}

/// A count of where a plan stands, for a driver's progress line and for the
/// starvation check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlanProgress {
    /// Every node.
    pub total: usize,
    /// Finished well.
    pub complete: usize,
    /// Finished badly.
    pub failed: usize,
    /// Dropped.
    pub abandoned: usize,
    /// In flight.
    pub assigned: usize,
    /// Pending, and able to start now.
    pub ready: usize,
    /// Pending, waiting on something in flight.
    pub waiting: usize,
}

impl PlanProgress {
    /// Whether nothing can move without an in-flight task finishing first.
    #[must_use]
    pub fn is_stalled(self) -> bool {
        self.ready == 0 && self.assigned == 0 && self.waiting > 0
    }
}

/// Count where a plan stands.
#[must_use]
pub fn progress(plan: &TaskPlan) -> PlanProgress {
    let mut out = PlanProgress {
        total: plan.len(),
        ..PlanProgress::default()
    };
    for node in plan.nodes() {
        match &node.status {
            TaskStatus::Complete => out.complete += 1,
            TaskStatus::Failed { .. } => out.failed += 1,
            TaskStatus::Abandoned { .. } => out.abandoned += 1,
            TaskStatus::Assigned { .. } => out.assigned += 1,
            TaskStatus::Pending => match readiness(plan, node.id) {
                Ok(Readiness::Ready) => out.ready += 1,
                _ => out.waiting += 1,
            },
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::Confidence;
    use crate::ops::{complete_node, fail_node, seed};
    use crate::plan::{NodeSpec, PlanMode};

    fn lead() -> ActorId {
        ActorId::new("lead")
    }

    fn worker() -> ActorId {
        ActorId::new("worker")
    }

    fn done(findings: &str) -> HandoffArtifact {
        HandoffArtifact::new(findings, Confidence::new(0.8))
    }

    /// a → c, b standalone.
    fn chain() -> (TaskPlan, Vec<NodeId>) {
        let mut plan = TaskPlan::new("ship it", PlanMode::Light, lead());
        let specs = vec![
            NodeSpec::task("a", "do a"),
            NodeSpec::task("b", "do b"),
            NodeSpec::task("c", "do c").after(0),
        ];
        let seeded = seed(&mut plan, &lead(), "k1", &specs).unwrap();
        (plan, seeded.nodes)
    }

    #[test]
    fn only_roots_are_ready_at_the_start() {
        let (plan, nodes) = chain();
        assert_eq!(ready_nodes(&plan), vec![nodes[0], nodes[1]]);
        assert_eq!(
            readiness(&plan, nodes[2]).unwrap(),
            Readiness::WaitingOn(vec![nodes[0]])
        );
    }

    #[test]
    fn completing_a_dependency_opens_the_frontier() {
        let (mut plan, nodes) = chain();
        dispatch(&mut plan, nodes[0], &worker()).unwrap();
        complete_node(&mut plan, nodes[0], &worker(), done("a is done")).unwrap();
        assert_eq!(ready_nodes(&plan), vec![nodes[1], nodes[2]]);
    }

    #[test]
    fn an_in_flight_task_is_not_ready_again() {
        let (mut plan, nodes) = chain();
        dispatch(&mut plan, nodes[0], &worker()).unwrap();
        assert_eq!(ready_nodes(&plan), vec![nodes[1]]);
        let err = dispatch(&mut plan, nodes[0], &ActorId::new("other")).unwrap_err();
        assert!(matches!(err, PlanError::AlreadyAssigned { .. }));
    }

    #[test]
    fn dispatching_something_that_is_still_waiting_says_what_for() {
        let (mut plan, nodes) = chain();
        let err = dispatch(&mut plan, nodes[2], &worker()).unwrap_err();
        assert_eq!(
            err,
            PlanError::NotReady {
                node: nodes[2],
                waiting_on: vec![nodes[0]]
            }
        );
    }

    #[test]
    fn a_failed_dependency_blocks_rather_than_waits() {
        let (mut plan, nodes) = chain();
        dispatch(&mut plan, nodes[0], &worker()).unwrap();
        fail_node(&mut plan, nodes[0], &worker(), "the build never passed").unwrap();
        assert_eq!(
            readiness(&plan, nodes[2]).unwrap(),
            Readiness::Blocked(vec![nodes[0]])
        );
        assert_eq!(blocked_nodes(&plan), vec![nodes[2]]);
        assert!(matches!(
            dispatch(&mut plan, nodes[2], &worker()),
            Err(PlanError::Blocked { .. })
        ));
    }

    #[test]
    fn cascading_settles_a_whole_stranded_tail() {
        let mut plan = TaskPlan::new("ship it", PlanMode::Light, lead());
        let specs = vec![
            NodeSpec::task("a", ""),
            NodeSpec::task("b", "").after(0),
            NodeSpec::task("c", "").after(1),
        ];
        let nodes = seed(&mut plan, &lead(), "k1", &specs).unwrap().nodes;
        dispatch(&mut plan, nodes[0], &worker()).unwrap();
        fail_node(&mut plan, nodes[0], &worker(), "boom").unwrap();

        let abandoned = cascade_blocked(&mut plan);
        assert_eq!(abandoned, vec![nodes[1], nodes[2]]);
        assert!(plan.is_settled());
    }

    #[test]
    fn an_assembled_input_carries_the_objective_and_upstream_handoffs() {
        let (mut plan, nodes) = chain();
        dispatch(&mut plan, nodes[0], &worker()).unwrap();
        complete_node(
            &mut plan,
            nodes[0],
            &worker(),
            done("the parser is in src/parse.rs").with_evidence("src/parse.rs:40"),
        )
        .unwrap();

        let input = assemble_input(&plan, nodes[2]).unwrap();
        assert!(input.contains("Objective: ship it"));
        assert!(input.contains("Your task"));
        assert!(input.contains("do c"));
        assert!(input.contains("the parser is in src/parse.rs"));
        assert!(input.contains("src/parse.rs:40"));
    }

    #[test]
    fn a_root_input_has_no_upstream_section() {
        let (plan, nodes) = chain();
        let input = assemble_input(&plan, nodes[0]).unwrap();
        assert!(!input.contains("What earlier tasks established"));
    }

    #[test]
    fn an_assignment_records_the_plan_version_it_was_cut_from() {
        let (mut plan, nodes) = chain();
        let assignment = dispatch(&mut plan, nodes[0], &worker()).unwrap();
        assert_eq!(assignment.plan_version, plan.version());
        assert_eq!(assignment.assignee, worker());
    }

    #[test]
    fn progress_counts_every_state() {
        let (mut plan, nodes) = chain();
        dispatch(&mut plan, nodes[0], &worker()).unwrap();
        complete_node(&mut plan, nodes[0], &worker(), done("done")).unwrap();
        dispatch(&mut plan, nodes[1], &worker()).unwrap();
        let p = progress(&plan);
        assert_eq!(p.total, 3);
        assert_eq!(p.complete, 1);
        assert_eq!(p.assigned, 1);
        assert_eq!(p.ready, 1);
        assert!(!p.is_stalled());
    }

    #[test]
    fn an_edge_to_a_missing_node_blocks_instead_of_running_without_input() {
        let (mut plan, nodes) = chain();
        // Simulate a corrupted snapshot: `c` waits on an id nothing minted.
        if let Some(node) = plan.node_mut(nodes[2]) {
            node.upstream = vec![NodeId::forged(9999)];
        }
        assert!(matches!(
            readiness(&plan, nodes[2]).unwrap(),
            Readiness::Blocked(_)
        ));
    }
}
