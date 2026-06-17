//! The tool-call budget stops a runaway tool loop cleanly with a recorded
//! reason, while a normal task stays well under the ceiling.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use localpilot_harness::{SessionConfig, SessionRuntime, StopReason};
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
            tool_call_budget: soft_start,
            tool_call_budget_max: hard_max,
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
fn a_normal_task_stays_under_the_default_budget() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("f.txt"), "x\n").unwrap();

    // One tool call then a final answer — far under the default ceiling.
    let provider = FakeProvider::new()
        .tool_call("c1", "read_file", json!({ "path": "f.txt" }))
        .text("the file says x");

    let mut runtime = runtime(root, provider, SessionConfig::default().tool_call_budget);
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
