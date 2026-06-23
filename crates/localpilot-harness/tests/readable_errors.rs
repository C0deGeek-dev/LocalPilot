//! Phase 1: model-readable validation errors.
//!
//! Drives the real session loop offline with a scripted [`FakeProvider`] and
//! asserts the observable contract: a shape-invalid tool call is answered with a
//! concise, schema-aware message (not the raw serde blob) when readable errors
//! are on; the raw message is restored when off (the rollback); a valid call is
//! byte-unaffected; the model recovers on the next call after a readable error
//! (no repair engine); and a secret-bearing argument never leaks into the message.
#![allow(clippy::unwrap_used)]

use std::process::Command;
use std::sync::Arc;

use localpilot_core::ContentBlock;
use localpilot_harness::{SessionConfig, SessionRuntime};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::{SessionEventKind, Store};
use localpilot_tools::ToolRegistry;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// One tool result surfaced into the transcript.
struct ResultRecord {
    id: String,
    output: String,
    is_error: bool,
}

/// Run one scripted turn and return the tool results plus the raw event log.
fn run_turn(
    script: &[(&str, &str, Value)],
    final_text: &str,
    readable_errors: bool,
    git_init: bool,
) -> (Vec<ResultRecord>, Vec<localpilot_store::SessionEvent>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("notes.txt"), "the answer is plumbus\n").unwrap();
    if git_init {
        let git = |args: &[&str]| {
            assert!(Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .unwrap()
                .success());
        };
        git(&["init"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "T"]);
        git(&["add", "-A"]);
        git(&["commit", "-m", "initial"]);
    }

    let mut provider = FakeProvider::new();
    for (tool, id, input) in script {
        provider = provider.tool_call(id, tool, input.clone());
    }
    provider = provider.text(final_text);

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
            enforce_readable_errors: readable_errors,
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    let session = runtime.session_id();
    let (events_tx, _rx) = broadcast::channel(256);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async { runtime.run_turn("do the task", &events_tx, &cancel).await });

    let events = Store::open(root).read_events(session).unwrap();
    let mut results = Vec::new();
    for event in &events {
        if let SessionEventKind::Message { message, .. } = &event.kind {
            for block in &message.content {
                if let ContentBlock::ToolResult(result) = block {
                    results.push(ResultRecord {
                        id: result.id.as_str().to_string(),
                        output: result.output.clone(),
                        is_error: result.is_error,
                    });
                }
            }
        }
    }
    (results, events)
}

fn result_for<'a>(results: &'a [ResultRecord], id: &str) -> &'a ResultRecord {
    results
        .iter()
        .find(|r| r.id == id)
        .unwrap_or_else(|| panic!("a result for {id} was recorded"))
}

fn retry_messages_sent(events: &[localpilot_store::SessionEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e.kind, SessionEventKind::ToolInputRetryMessageSent { .. }))
        .count()
}

#[test]
fn a_shape_invalid_call_gets_a_schema_aware_message_when_readable_errors_are_on() {
    let script = &[("git_diff", "c1", json!({ "paths": "README.md" }))];
    let (results, events) = run_turn(script, "done", true, false);
    let result = result_for(&results, "c1");
    assert!(result.is_error);
    assert!(
        result.output.contains("did not match its schema") && result.output.contains("array"),
        "the model gets a schema-aware message, not the raw serde blob: {}",
        result.output
    );
    assert!(
        !result.output.contains("invalid input:"),
        "the raw deserializer string is not the model-facing message"
    );
    assert_eq!(
        retry_messages_sent(&events),
        1,
        "the readable-error rung records that a correction was sent"
    );
}

#[test]
fn readable_errors_off_restores_the_raw_message_as_the_rollback() {
    let script = &[("git_diff", "c1", json!({ "paths": "README.md" }))];
    let (results, events) = run_turn(script, "done", false, false);
    let result = result_for(&results, "c1");
    assert!(result.is_error);
    assert!(
        result.output.contains("invalid input"),
        "with readable_errors off, the raw serde message is restored: {}",
        result.output
    );
    assert_eq!(
        retry_messages_sent(&events),
        0,
        "no readable-error rung fires when the feature is off"
    );
}

#[test]
fn a_valid_call_is_byte_unaffected_and_fires_no_readable_error() {
    let script = &[("read_file", "c1", json!({ "path": "notes.txt" }))];
    let (results, events) = run_turn(script, "done", true, false);
    let result = result_for(&results, "c1");
    assert!(
        !result.is_error,
        "the valid read succeeds: {}",
        result.output
    );
    assert!(
        result.output.contains("plumbus"),
        "the file content is read"
    );
    assert!(!result.output.contains("did not match its schema"));
    assert_eq!(retry_messages_sent(&events), 0);
}

#[test]
fn the_model_recovers_on_the_next_call_after_a_readable_error_without_repair() {
    // Call 1 omits the required `path` (invalid); call 2 supplies it (valid). The
    // readable error alone lets the model self-correct — no repair engine.
    let script = &[
        ("read_file", "c1", json!({})),
        ("read_file", "c2", json!({ "path": "notes.txt" })),
    ];
    let (results, _events) = run_turn(script, "the file says plumbus", true, false);
    let first = result_for(&results, "c1");
    let second = result_for(&results, "c2");
    assert!(first.is_error, "the malformed first call is rejected");
    assert!(first.output.contains("`path` is required"));
    assert!(
        !second.is_error && second.output.contains("plumbus"),
        "the corrected second call succeeds: {}",
        second.output
    );
}

#[test]
fn a_secret_bearing_argument_never_leaks_into_the_readable_message() {
    let secret = "sk-abcdefghijklmnopqrstuvwxyz0123";
    let script = &[("git_diff", "c1", json!({ "paths": secret }))];
    let (results, _events) = run_turn(script, "done", true, false);
    let result = result_for(&results, "c1");
    assert!(result.is_error);
    assert!(
        !result.output.contains(secret),
        "the schema-aware message must never echo the raw argument value: {}",
        result.output
    );
}
