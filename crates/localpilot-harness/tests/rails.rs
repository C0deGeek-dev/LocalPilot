//! The built-in safety rails (D003): an empty/minimal config self-bounds, so a
//! fresh project never runs an unbounded, externally-killed loop. This pins the
//! config→runtime wiring — `HarnessConfig::resolved_rails` fills a conservative
//! bound, and a runtime built with it stops a runaway with a recorded reason and
//! a parseable handoff instead of spinning.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use localpilot_config::{HarnessConfig, DEFAULT_HEADLESS_TOOL_BUDGET_MAX};
use localpilot_harness::{SessionConfig, SessionRuntime, StopReason};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use serde_json::json;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

#[test]
fn an_empty_config_self_bounds_a_runaway_headless_turn() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("f.txt"), "x\n").unwrap();

    // The defect this guards: an empty `.localpilot.toml` leaves budget+timeout
    // unset. The resolver fills the headless ceiling, so a runaway that would
    // otherwise run unbounded stops at it.
    let rails = HarnessConfig::default().resolved_rails(false);
    assert_eq!(
        rails.tool_call_budget_max,
        Some(DEFAULT_HEADLESS_TOOL_BUDGET_MAX),
        "an empty config must resolve to a bounded headless ceiling"
    );

    // A model that never stops calling: more calls than the ceiling allows.
    let mut provider = FakeProvider::new();
    for _ in 0..(DEFAULT_HEADLESS_TOOL_BUDGET_MAX + 5) {
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
            tool_call_budget: rails.tool_call_budget,
            tool_call_budget_max: rails.tool_call_budget_max,
            turn_timeout: rails.turn_timeout_secs.map(std::time::Duration::from_secs),
            ..SessionConfig::default()
        },
        Vec::new(),
    );

    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn("read forever", &events, &cancel));

    // The turn stops itself with a recorded cost-ceiling reason, and a parseable
    // handoff is available — never an unbounded loop run to an external kill.
    assert_eq!(reason, StopReason::BudgetExceeded);
    let handoff = runtime
        .last_turn_handoff()
        .expect("a handoff at the single exit");
    assert_eq!(handoff.reason, StopReason::BudgetExceeded);
    assert!(handoff
        .to_json_line()
        .contains("\"stop\":\"BudgetExceeded\""));
}

#[test]
fn an_interactive_empty_config_bounds_tool_calls_without_a_wall_clock() {
    // The interactive profile bounds a runaway tool loop (higher ceiling) but
    // sets no default wall-clock — a long interactive turn is legitimate.
    let rails = HarnessConfig::default().resolved_rails(true);
    assert!(rails.tool_call_budget_max.unwrap() >= DEFAULT_HEADLESS_TOOL_BUDGET_MAX);
    assert_eq!(rails.turn_timeout_secs, None);
}
