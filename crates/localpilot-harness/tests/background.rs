//! End-to-end wiring for `run_background`: the session exposes the process
//! registry to the tool, a started process is remembered across tool calls in a
//! turn, and closing the session terminates and forgets it.
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

fn runtime(root: &std::path::Path, provider: FakeProvider) -> SessionRuntime {
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
            ..SessionConfig::default()
        },
        Vec::new(),
    )
}

/// A command that prints a line and stays alive well past the turn.
fn stays_up_command() -> String {
    #[cfg(windows)]
    {
        "Write-Output ready; Start-Sleep -Seconds 30".to_string()
    }
    #[cfg(not(windows))]
    {
        "echo ready; sleep 30".to_string()
    }
}

/// Drain every currently buffered event from a receiver.
fn drain(rx: &mut broadcast::Receiver<RuntimeEvent>) -> Vec<RuntimeEvent> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

/// The `output` of the `ToolFinished` event for tool call `id`, asserting it did
/// not error.
fn tool_output(events: &[RuntimeEvent], id: &str) -> String {
    for event in events {
        if let RuntimeEvent::ToolFinished {
            id: got,
            is_error,
            output,
            ..
        } = event
        {
            if got == id {
                assert!(!is_error, "tool call `{id}` errored: {output}");
                return output.clone();
            }
        }
    }
    panic!("no ToolFinished event for `{id}`");
}

#[test]
fn a_started_process_is_remembered_then_killed_on_close() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Turn 1: start a long-running process, then list it. Turn 2 (after close):
    // list again — the registry must be empty.
    let provider = FakeProvider::new()
        .tool_call(
            "c1",
            "run_background",
            json!({ "command": stays_up_command(), "grace_secs": 0 }),
        )
        .tool_call("c2", "run_background", json!({ "action": "list" }))
        .text("running")
        .tool_call("c3", "run_background", json!({ "action": "list" }))
        .text("done");

    let mut runtime = runtime(root, provider);
    let (events, mut rx) = broadcast::channel(256);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();

    let reason = rt.block_on(runtime.run_turn("start the server", &events, &cancel));
    assert_eq!(reason, StopReason::Done);

    let turn1 = drain(&mut rx);
    let start = tool_output(&turn1, "c1");
    assert!(
        start.contains("started background process"),
        "start should report a tracked process: {start}"
    );
    let listed = tool_output(&turn1, "c2");
    assert!(
        listed.contains("running"),
        "the started process is remembered and listed as running: {listed}"
    );

    // Closing the session terminates and forgets every background process.
    runtime.close();

    let reason = rt.block_on(runtime.run_turn("list again", &events, &cancel));
    assert_eq!(reason, StopReason::Done);
    let after_close = tool_output(&drain(&mut rx), "c3");
    assert!(
        after_close.contains("no background processes"),
        "close() must terminate and forget the process: {after_close}"
    );
}
