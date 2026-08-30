//! Wiring tests for tool contract preconditions.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::Arc;

use localpilot_core::ContentBlock;
use localpilot_harness::{SessionConfig, SessionRuntime};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::{SessionEventKind, Store};
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

#[test]
fn edit_file_requires_an_existing_target_and_runs_when_it_exists() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let provider = FakeProvider::new()
        .tool_call(
            "c1",
            "edit_file",
            json!({ "path": "missing.txt", "old_text": "old", "new_text": "new" }),
        )
        .tool_call(
            "c2",
            "write_file",
            json!({ "path": "present.txt", "content": "old\n" }),
        )
        .tool_call(
            "c3",
            "edit_file",
            json!({ "path": "present.txt", "old_text": "old", "new_text": "new" }),
        )
        .text("done");

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
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    let session = runtime.session_id();
    let (events, _rx) = broadcast::channel(32);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async { runtime.run_turn("Edit files", &events, &cancel).await });

    let recorded = Store::open(root).read_events(session).unwrap();
    let blocked = recorded.iter().any(|event| {
        let SessionEventKind::Message { message, .. } = &event.kind else {
            return false;
        };
        message.content.iter().any(|block| {
            matches!(block, ContentBlock::ToolResult(result)
                if result.id.as_str() == "c1"
                    && result.is_error()
                    && result.output.contains("must be an existing file"))
        })
    });
    assert!(blocked, "the false state predicate must refuse the call");
    assert!(!root.join("missing.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("present.txt")).unwrap(),
        "new\n",
        "the state precondition must allow edit_file once its target exists"
    );
}
