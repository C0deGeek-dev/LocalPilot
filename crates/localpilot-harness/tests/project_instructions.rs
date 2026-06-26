//! Project instruction files are injected into the turn context directly —
//! ungated and independent of the learning store — so a fresh checkout's
//! `CLAUDE.md`/`AGENTS.md`/`Navigator.md` reaches the model on every turn.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use localpilot_harness::{register_project_instructions_context, SessionConfig, SessionRuntime};
use localpilot_llm::{FakeProvider, ModelRequest};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Does any message in the request carry `needle` (the request folds the
/// injected context into a leading system message)?
fn request_contains(requests: &[ModelRequest], turn: usize, needle: &str) -> bool {
    requests.get(turn).is_some_and(|request| {
        request
            .messages
            .iter()
            .any(|m| serde_json::to_string(m).map_or(false, |s| s.contains(needle)))
    })
}

#[test]
fn claude_md_is_injected_every_turn_with_no_learning_store() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("CLAUDE.md"), "PROJECT RULE: prefer tabs").unwrap();

    let provider = Arc::new(FakeProvider::new().text("ok one").text("ok two"));
    let mut runtime = SessionRuntime::new(
        provider.clone(),
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
    );
    // No LocalMind context hook is registered — only the direct, ungated
    // instruction injection. The store is empty.
    register_project_instructions_context(root, true, 8_000, &mut runtime);

    let (events, _rx) = broadcast::channel(16);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();

    rt.block_on(runtime.run_turn("first", &events, &cancel));
    rt.block_on(runtime.run_turn("second", &events, &cancel));

    let requests = provider.requests();
    assert!(
        request_contains(&requests, 0, "PROJECT RULE: prefer tabs"),
        "the instruction text must reach the model on the first turn"
    );
    assert!(
        request_contains(&requests, 1, "PROJECT RULE: prefer tabs"),
        "and on every subsequent turn"
    );
}

#[test]
fn injection_is_off_when_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("CLAUDE.md"), "PROJECT RULE: prefer tabs").unwrap();

    let provider = Arc::new(FakeProvider::new().text("ok"));
    let mut runtime = SessionRuntime::new(
        provider.clone(),
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
    );
    // Disabled: nothing is injected (the opt-out).
    register_project_instructions_context(root, false, 8_000, &mut runtime);

    let (events, _rx) = broadcast::channel(16);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(runtime.run_turn("first", &events, &cancel));

    assert!(
        !request_contains(&provider.requests(), 0, "PROJECT RULE: prefer tabs"),
        "with injection disabled the instructions must not be sent"
    );
}
