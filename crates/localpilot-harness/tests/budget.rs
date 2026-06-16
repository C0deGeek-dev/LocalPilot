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

fn runtime(root: &std::path::Path, provider: FakeProvider, budget: usize) -> SessionRuntime {
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
            tool_call_budget: budget,
            ..SessionConfig::default()
        },
        Vec::new(),
    )
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
