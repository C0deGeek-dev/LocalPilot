//! The tool-call budget stops a runaway tool loop cleanly with a recorded
//! reason, while a normal task stays well under the ceiling.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use localpilot_harness::{RuntimeEvent, SessionConfig, SessionRuntime, StopReason};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use serde_json::json;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

fn runtime_budgets(
    root: &std::path::Path,
    provider: FakeProvider,
    soft_start: usize,
    hard_max: usize,
) -> SessionRuntime {
    SessionRuntime::new(
        Arc::new(provider),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Bypass, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(root),
        Workspace::new(root).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            interactivity: Interactivity::NonInteractive,
            trusted: true,
            tool_call_budget: Some(soft_start),
            tool_call_budget_max: Some(hard_max),
            ..SessionConfig::default()
        },
        Vec::new(),
    )
}

/// A flat fixed ceiling (`max == soft start`): the pre-adaptive behaviour, used
/// by the cost-ceiling tests.
fn runtime(root: &std::path::Path, provider: FakeProvider, budget: usize) -> SessionRuntime {
    runtime_budgets(root, provider, budget, budget)
}

#[test]
fn a_runaway_tool_loop_hits_the_budget_and_stops() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("f.txt"), "x\n").unwrap();

    // Five scripted read calls against a budget of three: the loop runs three,
    // then stops at the fourth before it executes.
    let mut provider = FakeProvider::new();
    for _ in 0..5 {
        provider = provider.tool_call("c", "read_file", json!({ "path": "f.txt" }));
    }
    provider = provider.text("done");

    let mut runtime = runtime(root, provider, 3);
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn("read it repeatedly", &events, &cancel));

    assert_eq!(
        reason,
        StopReason::BudgetExceeded,
        "the runaway loop must stop on the budget, not run unbounded"
    );
}

#[test]
fn a_normal_task_stays_under_a_configured_budget() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("f.txt"), "x\n").unwrap();

    // One tool call then a final answer — far under a configured ceiling of 50.
    let provider = FakeProvider::new()
        .tool_call("c1", "read_file", json!({ "path": "f.txt" }))
        .text("the file says x");

    let mut runtime = runtime(root, provider, 50);
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn("read the file", &events, &cancel));

    assert_eq!(
        reason,
        StopReason::Done,
        "a normal task finishes normally, not on the budget"
    );
}

#[test]
fn an_unset_budget_runs_a_runaway_loop_to_completion() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("f.txt"), "x\n").unwrap();

    // The same read repeats many times — a loop that a budget of 3 would stop.
    // With the budget left at its default (off), nothing pre-empts it: every
    // scripted call runs and the turn ends on its own final answer.
    let mut provider = FakeProvider::new();
    for _ in 0..40 {
        provider = provider.tool_call("c", "read_file", json!({ "path": "f.txt" }));
    }
    provider = provider.text("done");

    let mut runtime = SessionRuntime::new(
        Arc::new(provider),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Bypass, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(root),
        Workspace::new(root).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            interactivity: Interactivity::NonInteractive,
            trusted: true,
            // tool_call_budget / tool_call_budget_max left at their default None.
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn("read it repeatedly", &events, &cancel));

    assert_eq!(
        reason,
        StopReason::Done,
        "with the budget off, the turn is unbounded and ends on its own answer"
    );
}

#[test]
fn a_productive_turn_runs_past_the_soft_start_when_the_max_is_higher() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // Five *distinct* reads (different files, different contents): each call
    // makes progress, so the no-progress detector never trips and the turn runs
    // past a soft start of 3 up to the higher max of 10.
    let mut provider = FakeProvider::new();
    for i in 0..5 {
        let name = format!("f{i}.txt");
        std::fs::write(root.join(&name), format!("contents {i}\n")).unwrap();
        provider = provider.tool_call(&format!("c{i}"), "read_file", json!({ "path": name }));
    }
    provider = provider.text("read them all");

    let mut runtime = runtime_budgets(root, provider, 3, 10);
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn("read each file", &events, &cancel));

    assert_eq!(
        reason,
        StopReason::Done,
        "a turn that keeps making progress runs past the old fixed ceiling"
    );
}

#[test]
fn a_spinning_turn_stops_on_no_progress_before_the_max() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("f.txt"), "x\n").unwrap();

    // The same read repeats with the same result: no forward progress. With a
    // soft start of 3 and a high max of 50, the turn stops on the no-progress
    // path at the soft start rather than spinning up to the cost ceiling.
    let mut provider = FakeProvider::new();
    for _ in 0..6 {
        provider = provider.tool_call("c", "read_file", json!({ "path": "f.txt" }));
    }
    provider = provider.text("done");

    let mut runtime = runtime_budgets(root, provider, 3, 50);
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn("read it repeatedly", &events, &cancel));

    assert_eq!(
        reason,
        StopReason::NoProgress,
        "a spinning turn stops on no progress, distinct from the cost ceiling"
    );
}

// --- A/B measurement: adaptive controller vs the flat fixed ceiling ----------

/// One scenario's outcome: the stop reason and whether the no-progress nudge
/// fired during the turn.
struct AbOutcome {
    reason: StopReason,
    nudged: bool,
}

/// Run `provider` to completion under a `(soft, max)` budget and report the stop
/// reason plus whether the no-progress strategy-change nudge was emitted.
fn run_ab(root: &std::path::Path, provider: FakeProvider, soft: usize, max: usize) -> AbOutcome {
    let mut runtime = runtime_budgets(root, provider, soft, max);
    let (events, mut rx) = broadcast::channel(4096);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn("task", &events, &cancel));
    let mut nudged = false;
    while let Ok(event) = rx.try_recv() {
        if let RuntimeEvent::Warning(text) = event {
            if text.contains("not making forward progress") {
                nudged = true;
            }
        }
    }
    AbOutcome { reason, nudged }
}

/// A provider that reads `count` *distinct* files (each written with unique
/// contents), then answers — a turn that keeps making progress.
fn distinct_reads(root: &std::path::Path, count: usize) -> FakeProvider {
    let mut provider = FakeProvider::new();
    for i in 0..count {
        let name = format!("f{i}.txt");
        std::fs::write(root.join(&name), format!("contents {i}\n")).unwrap();
        provider = provider.tool_call(&format!("c{i}"), "read_file", json!({ "path": name }));
    }
    provider.text("read them all")
}

/// A provider that reads the *same* file `count` times — a spinning turn that
/// makes no forward progress.
fn identical_reads(root: &std::path::Path, count: usize) -> FakeProvider {
    std::fs::write(root.join("f.txt"), "x\n").unwrap();
    let mut provider = FakeProvider::new();
    for _ in 0..count {
        provider = provider.tool_call("c", "read_file", json!({ "path": "f.txt" }));
    }
    provider.text("done")
}

/// Compare the adaptive controller against the flat fixed ceiling across three
/// scenarios, print the delta, and assert the wins: a productive turn the fixed
/// ceiling would cut now completes; a spinning turn stops on a distinct
/// `NoProgress` reason after a nudge; a runaway that defeats the no-progress
/// signal is still bounded by the hard cost ceiling.
#[test]
fn adaptive_vs_fixed_ceiling_ab() {
    const SOFT: usize = 8;
    const MAX: usize = 50;

    let dir = tempfile::tempdir().unwrap();

    // (a) Productive: 12 distinct reads — more than the soft start, all progress.
    let prod_root = dir.path().join("productive");
    std::fs::create_dir_all(&prod_root).unwrap();
    let fixed_prod = run_ab(&prod_root, distinct_reads(&prod_root, 12), SOFT, SOFT);
    let adaptive_prod = run_ab(&prod_root, distinct_reads(&prod_root, 12), SOFT, MAX);

    // (b) Spinning: 12 identical reads — no forward progress.
    let spin_root = dir.path().join("spinning");
    std::fs::create_dir_all(&spin_root).unwrap();
    let fixed_spin = run_ab(&spin_root, identical_reads(&spin_root, 12), SOFT, SOFT);
    let adaptive_spin = run_ab(&spin_root, identical_reads(&spin_root, 12), SOFT, MAX);

    // (c) Runaway that defeats the no-progress signal: 60 distinct reads.
    let run_root = dir.path().join("runaway");
    std::fs::create_dir_all(&run_root).unwrap();
    let adaptive_runaway = run_ab(&run_root, distinct_reads(&run_root, 60), SOFT, MAX);

    eprintln!("adaptive-budget A/B (soft={SOFT}, max={MAX}):");
    eprintln!(
        "  productive(12 distinct): fixed={:?} adaptive={:?}",
        fixed_prod.reason, adaptive_prod.reason
    );
    eprintln!(
        "  spinning(12 identical):  fixed={:?} adaptive={:?} (nudged={})",
        fixed_spin.reason, adaptive_spin.reason, adaptive_spin.nudged
    );
    eprintln!(
        "  runaway(60 distinct):    adaptive={:?}",
        adaptive_runaway.reason
    );

    // False stops of a productive turn: the fixed ceiling cuts it; the adaptive
    // controller does not.
    let fixed_false_stops = usize::from(fixed_prod.reason != StopReason::Done);
    let adaptive_false_stops = usize::from(adaptive_prod.reason != StopReason::Done);
    eprintln!(
        "  productive false-stops: fixed={fixed_false_stops} adaptive={adaptive_false_stops}"
    );

    // The win on a productive turn: fixed cuts it at the ceiling, adaptive runs.
    assert_eq!(fixed_prod.reason, StopReason::BudgetExceeded);
    assert_eq!(adaptive_prod.reason, StopReason::Done);
    assert_eq!(
        adaptive_false_stops, 0,
        "adaptive must not cut a productive turn"
    );

    // The spinning turn: fixed stops on the generic ceiling; adaptive stops on a
    // distinct, diagnosable reason and nudged a strategy change first.
    assert_eq!(fixed_spin.reason, StopReason::BudgetExceeded);
    assert_eq!(adaptive_spin.reason, StopReason::NoProgress);
    assert!(
        adaptive_spin.nudged,
        "a spinning turn gets a strategy-change nudge"
    );

    // The cost contract holds even when the no-progress signal is defeated.
    assert_eq!(
        adaptive_runaway.reason,
        StopReason::BudgetExceeded,
        "a runaway that defeats the no-progress signal is still bounded by the cost ceiling"
    );
}
