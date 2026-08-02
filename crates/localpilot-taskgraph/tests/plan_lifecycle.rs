// Shared helpers here are plain functions rather than `#[test]`s, so the
// workspace's test relaxation does not reach them; a failed setup step in a test
// should panic loudly anyway.
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! End-to-end scenarios over the task-graph engine — still with no live agents.
//!
//! These are the safety net the engine ships behind: whole plans driven from
//! seed to settled through the public API only, asserting the properties a
//! multi-agent run depends on. Every one of them is a failure that would be
//! nearly undiagnosable once real workers are attached, because a mis-scheduled
//! plan and an unhelpful model look identical from the outside.

use localpilot_taskgraph::ops::{
    abandon_node, complete_node, expand_node, fail_node, inject_from_gate, salvage_actor, seed,
    Salvage,
};
use localpilot_taskgraph::schedule::{
    assemble_input, blocked_nodes, cascade_blocked, dispatch, progress, ready_nodes,
};
use localpilot_taskgraph::sim::{simulate, SimAction, SimConfig};
use localpilot_taskgraph::{
    ActorId, Assignment, Confidence, HandoffArtifact, NodeId, NodeKind, NodeSpec, PlanError,
    PlanMode, TaskPlan, TaskStatus,
};

fn lead() -> ActorId {
    ActorId::new("coordinator")
}

fn worker(n: u8) -> ActorId {
    ActorId::new(format!("worker-{n}"))
}

/// A handoff that satisfies every rule, including a gate's.
fn full(findings: &str) -> HandoffArtifact {
    HandoffArtifact::new(findings, Confidence::new(0.8))
        .with_gap("anything outside the task as written")
        .with_validation("re-ran the checks named in the task")
        .with_evidence("crates/example/src/lib.rs:1")
}

/// survey → {left, right} → join
fn fan_out_specs() -> Vec<NodeSpec> {
    vec![
        NodeSpec::task("survey", "Find the two halves."),
        NodeSpec::task("left", "Do the left half.").after(0),
        NodeSpec::task("right", "Do the right half.").after(0),
        NodeSpec::task("join", "Combine both halves.")
            .after(1)
            .after(2),
    ]
}

fn seeded(mode: PlanMode) -> (TaskPlan, Vec<NodeId>, Option<NodeId>) {
    let mut plan = TaskPlan::new("make the suite green", mode, lead());
    let out = seed(&mut plan, &lead(), "seed-1", &fan_out_specs()).expect("the seed is valid");
    (plan, out.nodes, out.gate)
}

/// Finish one task cleanly as `actor`.
fn run(plan: &mut TaskPlan, node: NodeId, actor: &ActorId, findings: &str) {
    dispatch(plan, node, actor).expect("the task is ready");
    complete_node(plan, node, actor, full(findings)).expect("the handoff is complete");
}

#[test]
fn light_is_the_default_mode_and_inserts_no_gates() {
    assert_eq!(PlanMode::default(), PlanMode::Light);
    let (plan, nodes, gate) = seeded(PlanMode::Light);
    assert!(gate.is_none());
    assert_eq!(plan.len(), nodes.len());
    assert!(plan.nodes().all(|n| n.kind == NodeKind::Task));
}

#[test]
fn deep_mode_gates_the_seed_so_a_plan_cannot_finish_unreviewed() {
    let (plan, nodes, gate) = seeded(PlanMode::Deep);
    let gate = gate.expect("deep mode gates the seed");
    assert_eq!(plan.len(), nodes.len() + 1);
    assert_eq!(plan.node(gate).unwrap().kind, NodeKind::Gate);
    // The gate reviews the whole batch and nothing waits on the gate, so no
    // deep plan reaches a terminal state without passing through it.
    assert_eq!(plan.node(gate).unwrap().upstream, nodes);
    assert!(plan.dependents(gate).is_empty());
}

#[test]
fn a_retried_seed_replays_instead_of_doubling_the_plan() {
    let mut plan = TaskPlan::new("make the suite green", PlanMode::Deep, lead());
    let first = seed(&mut plan, &lead(), "seed-1", &fan_out_specs()).unwrap();
    let size = plan.len();
    let version = plan.version();

    // The same key with a *different* batch is still a replay: the key is the
    // caller saying "this is the same call", and honouring it is the whole point.
    let replay = seed(
        &mut plan,
        &lead(),
        "seed-1",
        &[NodeSpec::task("something else", "")],
    )
    .unwrap();

    assert!(replay.replayed);
    assert_eq!(replay.nodes, first.nodes);
    assert_eq!(replay.gate, first.gate);
    assert_eq!(plan.len(), size);
    assert_eq!(plan.version(), version);
}

#[test]
fn a_cycle_is_refused_and_leaves_the_plan_exactly_as_it_was() {
    let (mut plan, _nodes, _) = seeded(PlanMode::Light);
    let before = plan.clone();

    let looping = vec![
        NodeSpec::task("a", "").after(1),
        NodeSpec::task("b", "").after(0),
    ];
    let err = seed(&mut plan, &lead(), "seed-2", &looping).unwrap_err();

    assert!(matches!(err, PlanError::BatchCycle { .. }));
    assert_eq!(plan, before, "a refused mutation is not a partial mutation");
}

#[test]
fn a_gate_cannot_wave_work_through() {
    let (mut plan, nodes, gate) = seeded(PlanMode::Deep);
    let gate = gate.unwrap();
    run(
        &mut plan,
        nodes[0],
        &worker(1),
        "two halves, left and right",
    );
    run(&mut plan, nodes[1], &worker(1), "left half done");
    run(&mut plan, nodes[2], &worker(2), "right half done");
    run(&mut plan, nodes[3], &worker(1), "both halves combined");

    dispatch(&mut plan, gate, &lead()).unwrap();
    let waved = HandoffArtifact::new("looks good to me", Confidence::FULL).with_gap("n/a");
    assert!(matches!(
        complete_node(&mut plan, gate, &lead(), waved),
        Err(PlanError::RubberStampGate { .. })
    ));
    assert_eq!(
        plan.node(gate).unwrap().status,
        TaskStatus::Assigned { assignee: lead() },
        "a refused review leaves the gate open"
    );

    complete_node(&mut plan, gate, &lead(), full("every handoff checks out")).unwrap();
    assert!(plan.is_settled());
}

#[test]
fn a_gates_findings_become_work_and_the_gate_reviews_again() {
    let (mut plan, nodes, gate) = seeded(PlanMode::Deep);
    let gate = gate.unwrap();
    for (index, actor) in [(0, 1u8), (1, 1), (2, 2), (3, 1)] {
        run(&mut plan, nodes[index], &worker(actor), "done");
    }
    dispatch(&mut plan, gate, &lead()).unwrap();

    let injected = inject_from_gate(
        &mut plan,
        gate,
        &lead(),
        &[NodeSpec::task(
            "cover the error path",
            "The join never checks the failure branch. Cover it.",
        )],
    )
    .unwrap();

    assert_eq!(plan.node(gate).unwrap().status, TaskStatus::Pending);
    assert!(!plan.is_settled(), "the plan reopened");
    assert_eq!(ready_nodes(&plan), injected);

    run(&mut plan, injected[0], &worker(3), "error path covered");
    assert_eq!(ready_nodes(&plan), vec![gate]);
    dispatch(&mut plan, gate, &lead()).unwrap();
    complete_node(&mut plan, gate, &lead(), full("the finding is addressed")).unwrap();
    assert!(plan.is_settled());
}

#[test]
fn a_fan_outs_findings_reach_the_join_without_it_redoing_the_work() {
    let (mut plan, nodes, _) = seeded(PlanMode::Light);
    run(
        &mut plan,
        nodes[0],
        &worker(1),
        "the halves are parse and render",
    );
    dispatch(&mut plan, nodes[1], &worker(1)).unwrap();
    complete_node(
        &mut plan,
        nodes[1],
        &worker(1),
        full("parse is in crates/parse; the entry point is `parse::run`"),
    )
    .unwrap();
    dispatch(&mut plan, nodes[2], &worker(2)).unwrap();
    complete_node(
        &mut plan,
        nodes[2],
        &worker(2),
        full("render is in crates/render; it calls `parse::run` once"),
    )
    .unwrap();

    let input = assemble_input(&plan, nodes[3]).unwrap();
    assert!(
        input.contains("make the suite green"),
        "the objective travels"
    );
    assert!(input.contains("Combine both halves"), "the task travels");
    assert!(input.contains("the entry point is `parse::run`"));
    assert!(input.contains("it calls `parse::run` once"));
    assert!(
        !input.contains("the halves are parse and render"),
        "only the join's own upstream is hydrated, not the whole plan"
    );
}

#[test]
fn a_turn_that_produces_no_artifact_leaves_the_task_open_for_someone_else() {
    let (mut plan, nodes, _) = seeded(PlanMode::Deep);
    dispatch(&mut plan, nodes[0], &worker(1)).unwrap();

    // The worker answers without saying what it did not check: refused.
    let thin = HandoffArtifact::new("had a look", Confidence::FULL);
    assert!(matches!(
        complete_node(&mut plan, nodes[0], &worker(1), thin),
        Err(PlanError::IncompleteArtifact { .. })
    ));
    assert_eq!(
        plan.node(nodes[0]).unwrap().status,
        TaskStatus::Assigned {
            assignee: worker(1)
        }
    );

    // The worker then goes away. The task comes back to the pool, and a second
    // worker finishes it properly — the plan lost a turn, not the task.
    let salvaged = salvage_actor(&mut plan, &worker(1), 2);
    assert_eq!(
        salvaged,
        vec![(nodes[0], Salvage::Requeued { reclaims: 1 })]
    );
    assert_eq!(ready_nodes(&plan), vec![nodes[0]]);
    run(
        &mut plan,
        nodes[0],
        &worker(2),
        "the halves are parse and render",
    );
    assert_eq!(plan.node(nodes[0]).unwrap().status, TaskStatus::Complete);
}

#[test]
fn a_failure_strands_its_tail_and_the_plan_still_ends() {
    let (mut plan, nodes, _) = seeded(PlanMode::Light);
    run(&mut plan, nodes[0], &worker(1), "two halves");
    dispatch(&mut plan, nodes[1], &worker(1)).unwrap();
    fail_node(
        &mut plan,
        nodes[1],
        &worker(1),
        "the left half does not build",
    )
    .unwrap();
    run(&mut plan, nodes[2], &worker(2), "right half done");

    assert_eq!(blocked_nodes(&plan), vec![nodes[3]]);
    let abandoned = cascade_blocked(&mut plan);
    assert_eq!(abandoned, vec![nodes[3]]);
    assert!(plan.is_settled());

    let counts = progress(&plan);
    assert_eq!(counts.complete, 2);
    assert_eq!(counts.failed, 1);
    assert_eq!(counts.abandoned, 1);
}

#[test]
fn expansion_keeps_every_downstream_edge_intact() {
    let (mut plan, nodes, _) = seeded(PlanMode::Light);
    run(&mut plan, nodes[0], &worker(1), "two halves");
    dispatch(&mut plan, nodes[1], &worker(1)).unwrap();

    let expanded = expand_node(
        &mut plan,
        nodes[1],
        &worker(1),
        &[
            NodeSpec::task("left-parse", "Handle parsing."),
            NodeSpec::task("left-render", "Handle rendering.").after(0),
        ],
    )
    .unwrap();

    // The join still waits on the same two nodes it always did.
    assert_eq!(
        plan.node(nodes[3]).unwrap().upstream,
        vec![nodes[1], nodes[2]]
    );
    // And the children picked up the expanded node's own input.
    assert_eq!(
        plan.node(expanded.children[0]).unwrap().upstream,
        vec![nodes[0]]
    );

    run(&mut plan, expanded.children[0], &worker(1), "parsing done");
    run(
        &mut plan,
        expanded.children[1],
        &worker(1),
        "rendering done",
    );
    // The expanded node is now a join over its own children and runs again.
    assert!(ready_nodes(&plan).contains(&nodes[1]));
    let input = assemble_input(&plan, nodes[1]).unwrap();
    assert!(input.contains("parsing done") && input.contains("rendering done"));
}

#[test]
fn a_plan_survives_a_snapshot_round_trip_mid_flight() {
    let (mut plan, nodes, gate) = seeded(PlanMode::Deep);
    let gate = gate.unwrap();
    run(&mut plan, nodes[0], &worker(1), "two halves");
    dispatch(&mut plan, nodes[1], &worker(1)).unwrap();
    inject_from_gate(
        &mut plan,
        gate,
        &lead(),
        &[NodeSpec::task("extra", "one more thing")],
    )
    .unwrap();

    let json = serde_json::to_string(&plan).unwrap();
    let restored: TaskPlan = serde_json::from_str(&json).unwrap();

    assert_eq!(restored, plan);
    assert_eq!(restored.version(), plan.version());
    assert_eq!(ready_nodes(&restored), ready_nodes(&plan));
    // The in-flight assignment and the gate's re-opened debt both survived.
    assert_eq!(
        restored.node(nodes[1]).unwrap().status,
        TaskStatus::Assigned {
            assignee: worker(1)
        }
    );
    assert_eq!(
        restored.node(gate).unwrap().upstream.len(),
        nodes.len() + 1,
        "the whole seeded batch plus the injected remediation"
    );
}

#[test]
fn a_restored_plan_keeps_refusing_a_replayed_seed() {
    let (plan, nodes, _) = seeded(PlanMode::Light);
    let json = serde_json::to_string(&plan).unwrap();
    let mut restored: TaskPlan = serde_json::from_str(&json).unwrap();

    let replay = seed(&mut restored, &lead(), "seed-1", &fan_out_specs()).unwrap();
    assert!(replay.replayed, "idempotency has to survive a restart");
    assert_eq!(replay.nodes, nodes);
    assert_eq!(restored.len(), plan.len());
}

#[test]
fn a_deep_plan_with_expansion_and_a_gate_finding_runs_to_settled() {
    let mut plan = TaskPlan::new("make the suite green", PlanMode::Deep, lead());
    seed(&mut plan, &lead(), "seed-1", &fan_out_specs()).unwrap();

    let mut expanded_once = false;
    let mut injected_once = false;
    let report = simulate(
        &mut plan,
        &mut |assignment: &Assignment, _: &TaskPlan| {
            if assignment.title == "left" && !expanded_once {
                expanded_once = true;
                return SimAction::Expand(vec![
                    NodeSpec::task("left-a", "First part."),
                    NodeSpec::task("left-b", "Second part.").after(0),
                ]);
            }
            if assignment.kind == NodeKind::Gate && !injected_once {
                injected_once = true;
                return SimAction::Inject(vec![NodeSpec::task("remediate", "Close the gap.")]);
            }
            SimAction::Complete(Box::new(full(&format!("{} done", assignment.title))))
        },
        SimConfig::default(),
    );

    assert!(expanded_once && injected_once);
    assert!(report.settled, "{:?}", report.progress);
    assert!(!report.exhausted);
    assert!(report.refusals().is_empty(), "{:?}", report.refusals());
    assert_eq!(report.progress.complete, report.progress.total);
}

#[test]
fn owner_and_assignee_are_the_only_ones_who_may_change_a_task() {
    let (mut plan, nodes, _) = seeded(PlanMode::Light);
    dispatch(&mut plan, nodes[0], &worker(1)).unwrap();

    let intruder = ActorId::new("worker-9");
    assert!(matches!(
        expand_node(&mut plan, nodes[0], &intruder, &[NodeSpec::task("x", "")]),
        Err(PlanError::NotOwner { .. })
    ));
    assert!(matches!(
        complete_node(&mut plan, nodes[0], &intruder, full("mine now")),
        Err(PlanError::WrongAssignee { .. })
    ));
    // Abandoning is stricter still: the assignee cannot, only the owner.
    assert!(matches!(
        abandon_node(&mut plan, nodes[0], &worker(1), "giving up"),
        Err(PlanError::NotOwner { .. })
    ));
    abandon_node(&mut plan, nodes[0], &lead(), "the plan changed").unwrap();
}

#[test]
fn a_simulated_run_is_reproducible_from_the_same_seed() {
    let run_once = || {
        let mut plan = TaskPlan::new("make the suite green", PlanMode::Deep, lead());
        seed(&mut plan, &lead(), "seed-1", &fan_out_specs()).unwrap();
        let mut turn = 0usize;
        let report = simulate(
            &mut plan,
            &mut |assignment: &Assignment, _: &TaskPlan| {
                turn += 1;
                // A worker that vanishes on a fixed turn: deterministic, and it
                // drags the salvage path into the reproducibility guarantee.
                if turn == 2 {
                    return SimAction::Vanish;
                }
                SimAction::Complete(Box::new(full(&format!("{} done", assignment.title))))
            },
            SimConfig::default(),
        );
        (report, plan)
    };

    let (first_report, first_plan) = run_once();
    let (second_report, second_plan) = run_once();
    assert_eq!(first_report, second_report);
    assert_eq!(first_plan, second_plan);
    assert!(first_report.settled);
}
