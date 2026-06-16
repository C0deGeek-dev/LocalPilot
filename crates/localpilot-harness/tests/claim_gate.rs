//! The no-unsupported-claim gate, end to end: with the gate on, a final reply
//! that claims an action completed is flagged unless a verified tool call backs
//! it; a claim a verified call supports is left alone.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::Arc;

use localpilot_core::{ContentBlock, Role};
use localpilot_harness::{SessionConfig, SessionRuntime};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use serde_json::json;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Run one scripted turn with the claim gate on; return the final reply text as
/// it was persisted.
fn final_reply(root: &Path, provider: FakeProvider) -> String {
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
            enforce_claim_gate: true,
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    let session = runtime.session_id();
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async { runtime.run_turn("update the file", &events, &cancel).await });

    let transcript = Store::open(root).read_transcript(session).unwrap();
    transcript
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant && m.content.iter().any(is_text))
        .and_then(|m| {
            m.content.iter().rev().find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
        })
        .unwrap_or_default()
}

fn is_text(block: &ContentBlock) -> bool {
    matches!(block, ContentBlock::Text { .. })
}

#[test]
fn an_unsupported_claim_after_a_failed_write_is_flagged() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // The write escapes the workspace and fails; the model claims success anyway.
    let provider = FakeProvider::new()
        .tool_call(
            "c1",
            "write_file",
            json!({ "path": "../escape.txt", "content": "x\n" }),
        )
        .text("I created the file.");

    let reply = final_reply(root, provider);
    assert!(
        reply.contains("[unverified]"),
        "an action claim with no verified call must be flagged: {reply}"
    );
    assert!(reply.contains("I created the file."));
}

#[test]
fn a_claim_backed_by_a_verified_write_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // The write succeeds in the workspace; its postcondition (the file exists)
    // verifies, so the same claim is supported and untouched.
    let provider = FakeProvider::new()
        .tool_call(
            "c1",
            "write_file",
            json!({ "path": "out.txt", "content": "x\n" }),
        )
        .text("I created the file.");

    let reply = final_reply(root, provider);
    assert_eq!(
        reply, "I created the file.",
        "a verified action claim is left untouched"
    );
}
