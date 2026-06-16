//! Wiring test for the `RequiresPriorRead` contract precondition: when enforced,
//! a destructive overwrite of an existing, unread file is refused before it runs;
//! reading the file first lets the overwrite proceed.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::Arc;

use localpilot_harness::{SessionConfig, SessionRuntime};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use serde_json::json;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Run one scripted turn against a workspace with prior-read enforcement on.
fn run_turn_enforced(root: &Path, provider: FakeProvider) {
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
            enforce_prior_read: true,
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    let (events, _rx) = broadcast::channel(32);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async { runtime.run_turn("Update data.txt", &events, &cancel).await });
}

#[test]
fn overwrite_without_a_prior_read_is_blocked() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("data.txt"), "original\n").unwrap();

    let provider = FakeProvider::new()
        .tool_call(
            "c1",
            "write_file",
            json!({ "path": "data.txt", "content": "changed\n" }),
        )
        .text("done");
    run_turn_enforced(root, provider);

    assert_eq!(
        std::fs::read_to_string(root.join("data.txt")).unwrap(),
        "original\n",
        "the overwrite must be blocked: the file was not read this session"
    );
}

#[test]
fn overwrite_after_a_prior_read_proceeds() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("data.txt"), "original\n").unwrap();

    let provider = FakeProvider::new()
        .tool_call("c1", "read_file", json!({ "path": "data.txt" }))
        .tool_call(
            "c2",
            "write_file",
            json!({ "path": "data.txt", "content": "changed\n" }),
        )
        .text("done");
    run_turn_enforced(root, provider);

    assert_eq!(
        std::fs::read_to_string(root.join("data.txt")).unwrap(),
        "changed\n",
        "after reading the file, the overwrite proceeds"
    );
}
