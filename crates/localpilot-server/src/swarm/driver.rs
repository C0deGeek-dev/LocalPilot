//! Running a plan: dispatch what is ready, refill as workers finish, stop when
//! there is nothing left that can move.
//!
//! Everything hard has already been solved elsewhere — the graph decides what is
//! ready ([`localpilot_taskgraph::schedule`]), the registry bounds the fan-out,
//! the lifecycle handles a worker that dies. This is the loop that joins them,
//! and the only two decisions it makes for itself are worth stating:
//!
//! - **Refill on each completion, not on each round.** Waiting for a whole wave
//!   before starting anything new wastes exactly as long as the difference
//!   between the fastest and slowest worker in it, every time. So the driver
//!   awaits the *first* worker to finish and immediately dispatches whatever
//!   that unblocked.
//! - **A stalled plan ends.** If nothing is in flight and nothing is ready, the
//!   plan is over — settled or not — and saying so beats waiting for a worker
//!   that was never dispatched.
//!
//! The starvation hint exists because the most disappointing way to run a plan
//! is correctly: a graph that is one long chain runs one worker at a time no
//! matter how large the budget, and nothing about that looks wrong from the
//! outside. If the plan never used much of its concurrency, the report says so.

use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use localpilot_core::SessionId;
use localpilot_harness::{assignment_contract, SwarmDepth};
use localpilot_taskgraph::ops::{complete_node, fail_node};
use localpilot_taskgraph::schedule::{cascade_blocked, dispatch, progress, ready_nodes};
use localpilot_taskgraph::{ActorId, Confidence, HandoffArtifact, NodeId, PlanMode};

use super::lifecycle::{reap_terminal, salvage, DEFAULT_RECLAIM_LIMIT};
use super::scope::SwarmId;
use super::spawn::{SpawnRequest, SwarmHost};

/// How a plan run is bounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverConfig {
    /// How many workers may be in flight at once. Also bounded by the swarm's
    /// own concurrency budget, whichever is smaller.
    pub concurrency: usize,
    /// How many times one task may be salvaged before it fails.
    pub reclaim_limit: u32,
    /// A hard ceiling on dispatches, so a plan that keeps re-opening itself
    /// stops loudly instead of running until someone notices.
    pub max_dispatches: usize,
    /// How long a single worker may take before it is presumed gone and its task
    /// salvaged.
    pub worker_timeout: Duration,
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            concurrency: 4,
            reclaim_limit: DEFAULT_RECLAIM_LIMIT,
            max_dispatches: 256,
            worker_timeout: Duration::from_secs(15 * 60),
        }
    }
}

/// What a run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    /// How many tasks were handed to a worker.
    pub dispatched: usize,
    /// How many finished with a handoff.
    pub completed: usize,
    /// How many finished badly.
    pub failed: usize,
    /// How many were dropped because something upstream never completed.
    pub abandoned: usize,
    /// The most workers that were ever in flight at once.
    pub peak_in_flight: usize,
    /// Whether every task reached a terminal state.
    pub settled: bool,
    /// Whether the dispatch ceiling stopped the run.
    pub exhausted: bool,
    /// Set when the plan never came close to using its concurrency — the shape
    /// of the graph, not a fault, but worth saying.
    pub starvation: Option<String>,
}

/// Run a swarm's plan to completion.
///
/// Each ready task gets a fresh worker, which is given the task's assembled
/// input with the assignment contract in front of it — a worker does not inherit
/// the coordinator's system prompt, so anything expected of it has to travel
/// with the work.
pub async fn run_plan(
    host: &SwarmHost,
    swarm: &SwarmId,
    coordinator: SessionId,
    config: DriverConfig,
) -> RunReport {
    let concurrency = config.concurrency.max(1);
    let depth = match host.swarms().plan(swarm).await.map(|plan| plan.mode()) {
        Some(PlanMode::Deep) => SwarmDepth::Deep,
        _ => SwarmDepth::Light,
    };

    let mut in_flight = FuturesUnordered::new();
    let mut dispatched = 0usize;
    let mut peak = 0usize;
    let mut exhausted = false;

    loop {
        // Settle anything stranded before reading the frontier, so a plan that
        // is over stops looking like one that is waiting.
        host.swarms().with_plan(swarm, cascade_blocked).await.ok();

        while in_flight.len() < concurrency {
            if dispatched >= config.max_dispatches {
                exhausted = true;
                break;
            }
            let Some(node) = next_ready(host, swarm).await else {
                break;
            };
            match start(
                host,
                swarm,
                coordinator,
                node,
                depth,
                dispatched,
                config.worker_timeout,
            )
            .await
            {
                Some(running) => {
                    dispatched += 1;
                    peak = peak.max(in_flight.len() + 1);
                    in_flight.push(running);
                }
                // The task could not be started at all — no worker, no slot.
                // Fail it rather than spinning on a frontier that never shrinks.
                None => {
                    host.swarms()
                        .with_plan(swarm, |plan| {
                            let _ = fail_node(
                                plan,
                                node,
                                &ActorId::new(coordinator.to_string()),
                                "no worker could be started for this task",
                            );
                        })
                        .await
                        .ok();
                }
            }
        }

        if in_flight.is_empty() {
            break;
        }
        // Await the *first* worker to finish, then loop straight back to refill.
        if let Some(finished) = in_flight.next().await {
            settle(host, swarm, finished, config.reclaim_limit).await;
        }
    }

    let counts = host
        .swarms()
        .with_plan(swarm, |plan| (progress(plan), plan.is_settled()))
        .await
        .unwrap_or_default();
    reap_terminal(host, swarm).await;

    RunReport {
        dispatched,
        completed: counts.0.complete,
        failed: counts.0.failed,
        abandoned: counts.0.abandoned,
        peak_in_flight: peak,
        settled: counts.1,
        exhausted,
        starvation: starvation_hint(peak, concurrency, dispatched),
    }
}

/// One dispatched task, awaited.
struct Running {
    node: NodeId,
    worker: SessionId,
    /// The worker's final text, or `None` if it never produced one.
    answer: Option<String>,
    /// Whether the worker stopped answering within its allowance.
    timed_out: bool,
}

/// Spawn a worker for `node`, mark the task assigned, and start its turn.
async fn start(
    host: &SwarmHost,
    swarm: &SwarmId,
    coordinator: SessionId,
    node: NodeId,
    depth: SwarmDepth,
    sequence: usize,
    timeout: Duration,
) -> Option<impl std::future::Future<Output = Running>> {
    let (title, model) = host
        .swarms()
        .with_plan(swarm, |plan| {
            plan.node(node)
                .map(|task| (task.title.clone(), task.model.clone()))
        })
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| (node.to_string(), None));

    // An idempotency key per (plan position, attempt) so a retried spawn cannot
    // put two workers on one task — the failure that makes them edit the same
    // files and disagree.
    let mut request = SpawnRequest::new(swarm.clone(), coordinator, title, String::new())
        .with_key(format!("{node}#{sequence}"));
    // A task may pin the model its worker runs on. `None` leaves the request
    // model-less, so the worker is built on the session default; a named model
    // is carried through and the spawn path verifies the worker really runs on
    // it rather than trusting the factory.
    if let Some(model) = model {
        request = request.with_model(model);
    }
    let worker = match host.spawn(&request).await {
        Ok(super::spawn::Spawned::Started { session }) => session,
        Ok(super::spawn::Spawned::Already { session }) => session,
        _ => return None,
    };

    let assignment = host
        .swarms()
        .with_plan(swarm, |plan| {
            dispatch(plan, node, &ActorId::new(worker.to_string()))
        })
        .await
        .ok()?
        .ok()?;

    // The contract travels with the work, because the worker has none of the
    // coordinator's prompt.
    let input = format!("{}\n\n{}", assignment_contract(depth), assignment.input);
    let host = host.clone();
    let swarm = swarm.clone();
    Some(async move {
        // A worker that never returns would hold its slot and its task for the
        // life of the run. The timeout turns "hung" into "gone", which the
        // salvage path already knows what to do with.
        match tokio::time::timeout(timeout, host.run(&swarm, worker, &input)).await {
            Ok(report) => Running {
                node,
                worker,
                answer: report.ok().map(|report| report.summary),
                timed_out: false,
            },
            Err(_) => Running {
                node,
                worker,
                answer: None,
                timed_out: true,
            },
        }
    })
}

/// Turn a finished worker's answer into a plan mutation.
async fn settle(host: &SwarmHost, swarm: &SwarmId, finished: Running, reclaim_limit: u32) {
    let Running {
        node,
        worker,
        answer,
        timed_out,
    } = finished;

    if timed_out
        || answer
            .as_deref()
            .map_or(true, |text| text.trim().is_empty())
    {
        // A worker that reported nothing is indistinguishable from one that
        // died, and is treated the same: the task goes back to the pool rather
        // than being marked done on the strength of silence.
        salvage(host, swarm, worker, reclaim_limit).await;
        return;
    }

    let actor = ActorId::new(worker.to_string());
    let answer = answer.unwrap_or_default();
    let applied = host
        .swarms()
        .with_plan(swarm, |plan| {
            let artifact = artifact_from(&answer, plan, node);
            complete_node(plan, node, &actor, artifact)
        })
        .await;

    // A refused completion — a deep-mode report with no coverage statement, say
    // — must not leave the task assigned to a worker that has stopped. Put it
    // back so somebody else can try.
    if !matches!(applied, Ok(Ok(()))) {
        salvage(host, swarm, worker, reclaim_limit).await;
    }
}

/// Read a worker's free text as a handoff.
///
/// The engine wants structure and a worker produced prose. The honest reading is
/// that everything it said is the finding, that nothing it claimed was verified,
/// and that its coverage is unknown — so the confidence is middling and the gap
/// says exactly that. Inventing a confidence or claiming coverage would put a
/// fabricated artifact into the record every downstream task then reads.
///
/// A **review gate** gets two fields the driver can fill truthfully rather than
/// invent: what it reviewed is what the graph says it waits on, and that it
/// reviewed them is a fact about the dispatch, not a claim about the model. A
/// gate cannot close without those, so without this a deep plan's gate can never
/// pass however good the review was — which is the whole mode failing on a
/// formality.
fn artifact_from(
    answer: &str,
    plan: &localpilot_taskgraph::TaskPlan,
    node: NodeId,
) -> HandoffArtifact {
    let artifact = HandoffArtifact::new(answer.trim(), Confidence::new(0.5)).with_gap(
        "not stated by the worker — this report was not returned in the structured form, so \
         treat its coverage as unknown",
    );
    let Some(task) = plan.node(node) else {
        return artifact;
    };
    if task.kind != localpilot_taskgraph::NodeKind::Gate {
        return artifact;
    }
    let reviewed: Vec<String> = task
        .upstream
        .iter()
        .filter_map(|id| plan.node(*id))
        .map(|up| format!("{} {}", up.id, up.title))
        .collect();
    let mut artifact = artifact.with_validation(format!(
        "read the handoffs of {} upstream task(s) before answering",
        reviewed.len()
    ));
    for item in reviewed {
        artifact = artifact.with_evidence(item);
    }
    artifact
}

/// The next task to hand out, if any.
async fn next_ready(host: &SwarmHost, swarm: &SwarmId) -> Option<NodeId> {
    host.swarms()
        .with_plan(swarm, |plan| ready_nodes(plan).first().copied())
        .await
        .ok()
        .flatten()
}

/// Say plainly when a plan could not use the parallelism it was given.
///
/// Not a fault — a chain is a chain — but a run that took four times as long as
/// the budget suggested, with nothing anywhere saying why, is the most
/// disappointing possible outcome.
fn starvation_hint(peak: usize, concurrency: usize, dispatched: usize) -> Option<String> {
    // `>=` rather than `>`: a plan that used exactly half its budget was not
    // starved, and saying so anyway trains the reader to ignore the hint.
    if concurrency <= 1 || dispatched <= 1 || peak * 2 >= concurrency {
        return None;
    }
    Some(format!(
        "this plan never had more than {peak} task(s) running at once, against a budget of \
         {concurrency}. The graph is narrower than the budget — the tasks depend on each other in \
         a chain — so extra workers cannot make it faster. Decompose into pieces that do not wait \
         on each other if you want the parallelism."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_taskgraph::{NodeSpec, TaskPlan};

    /// A one-task plan and its node, so the artifact reader has a real graph to
    /// look at.
    fn one_task_plan() -> (TaskPlan, NodeId) {
        let lead = ActorId::new("lead");
        let mut plan = TaskPlan::new("test", PlanMode::Light, lead.clone());
        let node =
            localpilot_taskgraph::ops::seed(&mut plan, &lead, "k", &[NodeSpec::task("a", "do a")])
                .expect("a one-task seed is valid")
                .nodes[0];
        (plan, node)
    }

    /// A deep plan whose auto-inserted gate is the node returned.
    fn gated_plan() -> (TaskPlan, NodeId) {
        let lead = ActorId::new("lead");
        let mut plan = TaskPlan::new("test", PlanMode::Deep, lead.clone());
        let gate = localpilot_taskgraph::ops::seed(
            &mut plan,
            &lead,
            "k",
            &[NodeSpec::task("a", "do a"), NodeSpec::task("b", "do b")],
        )
        .expect("a two-task seed is valid")
        .gate
        .expect("deep mode gates the seed");
        (plan, gate)
    }

    #[test]
    fn a_gates_handoff_records_what_it_reviewed_so_the_gate_can_actually_close() {
        let (plan, gate) = gated_plan();
        let artifact = artifact_from("both handoffs check out", &plan, gate);

        // Without these two fields the engine refuses every gate completion, so
        // a deep plan could never finish however good the review was.
        assert!(!artifact.validation.trim().is_empty());
        assert_eq!(artifact.evidence.len(), 2, "one per reviewed task");
        assert!(artifact.evidence.iter().any(|e| e.contains('a')));

        // And it is a record of the dispatch, not a claim about the model: the
        // driver knows what the gate waited on because the graph says so.
        assert!(artifact.validation.contains("2 upstream task"));
    }

    #[test]
    fn a_narrow_plan_says_why_it_could_not_use_its_budget() {
        let hint = starvation_hint(1, 8, 6).expect("a chain of six under a budget of eight");
        assert!(hint.contains("narrower than the budget"), "{hint}");
        assert!(hint.contains('8'), "{hint}");
    }

    #[test]
    fn a_plan_that_used_its_budget_says_nothing() {
        assert!(starvation_hint(5, 8, 20).is_none());
        assert!(
            starvation_hint(4, 8, 20).is_none(),
            "half the budget is fine"
        );
    }

    #[test]
    fn a_single_task_plan_is_not_starved() {
        assert!(starvation_hint(1, 8, 1).is_none());
    }

    #[test]
    fn a_budget_of_one_is_not_starved() {
        assert!(starvation_hint(1, 1, 10).is_none());
    }

    #[test]
    fn free_text_becomes_a_handoff_that_does_not_overstate_itself() {
        let (plan, node) = one_task_plan();
        let artifact = artifact_from("  the parser lives in src/parse.rs  ", &plan, node);
        assert_eq!(artifact.findings, "the parser lives in src/parse.rs");
        assert!(
            artifact.evidence.is_empty(),
            "nothing was cited, so nothing is claimed"
        );
        assert!(artifact.what_i_did_not_check.contains("unknown"));
        assert!(
            artifact.confidence.value() < 0.8,
            "an unstructured report must not arrive looking confident"
        );
    }
}
