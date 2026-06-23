//! Phase 2: the validator-first argument-repair stage, end to end.
//!
//! Drives the real session loop offline and asserts the observable safety
//! contract: a safe, shape-invalid call is repaired and runs (with a model-visible
//! note); a destructive / external-write tool is **never** repaired (readable
//! error + an audited refusal); `repair = off` reproduces the pre-repair behaviour
//! exactly; and a valid call is untouched.
#![allow(clippy::unwrap_used)]

use std::process::Command;
use std::sync::Arc;

use localpilot_config::RepairMode;
use localpilot_core::ContentBlock;
use localpilot_harness::{SessionConfig, SessionRuntime};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::{SessionEvent, SessionEventKind, Store};
use localpilot_tools::ToolRegistry;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

struct ResultRecord {
    id: String,
    output: String,
    is_error: bool,
}

fn run_turn(
    script: &[(&str, &str, Value)],
    readable_errors: bool,
    repair_mode: RepairMode,
) -> (Vec<ResultRecord>, Vec<SessionEvent>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("notes.txt"), "the answer is plumbus\n").unwrap();
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

    let mut provider = FakeProvider::new();
    for (tool, id, input) in script {
        provider = provider.tool_call(id, tool, input.clone());
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
            enforce_readable_errors: readable_errors,
            repair_mode,
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

fn repaired_events(events: &[SessionEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e.kind, SessionEventKind::ToolInputRepaired { .. }))
        .count()
}

fn high_risk_refusals(events: &[SessionEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e.kind, SessionEventKind::ToolRepairRejectedHighRisk { .. }))
        .count()
}

#[test]
fn repair_warn_fixes_a_bare_string_path_list_and_notes_it() {
    // git_diff is read-only → repair-eligible. A bare-string `paths` is wrapped.
    let script = &[("git_diff", "c1", json!({ "paths": "notes.txt" }))];
    let (results, events) = run_turn(script, true, RepairMode::Warn);
    let result = result_for(&results, "c1");
    assert!(
        !result.is_error,
        "the repaired diff runs: {}",
        result.output
    );
    assert!(
        result.output.contains("[arguments repaired]"),
        "the model is told what was changed: {}",
        result.output
    );
    assert_eq!(repaired_events(&events), 1, "a repair is recorded");
    assert_eq!(high_risk_refusals(&events), 0);
}

#[test]
fn a_stringified_array_is_repaired_when_the_tool_is_safe() {
    let script = &[("git_add", "c1", json!({ "paths": "[\"notes.txt\"]" }))];
    let (results, events) = run_turn(script, true, RepairMode::On);
    let result = result_for(&results, "c1");
    assert!(!result.is_error, "the repaired add runs: {}", result.output);
    assert_eq!(repaired_events(&events), 1);
}

#[test]
fn a_destructive_tool_is_never_repaired() {
    // git_restore is destructive → refused: readable error, audited refusal, no
    // silent rewrite of an invalid destructive call into a valid one.
    let script = &[("git_restore", "c1", json!({ "paths": "notes.txt" }))];
    let (results, events) = run_turn(script, true, RepairMode::On);
    let result = result_for(&results, "c1");
    assert!(result.is_error);
    assert!(
        result.output.contains("did not match its schema"),
        "a refused tool gets the readable error, not a repair: {}",
        result.output
    );
    assert_eq!(repaired_events(&events), 0, "nothing was repaired");
    assert_eq!(high_risk_refusals(&events), 1, "the refusal is audited");
}

#[test]
fn the_named_high_risk_tools_are_never_repaired() {
    // The safety contract: an invalid destructive / external-write call stays
    // invalid even with repair on — run_shell, an apply_patch delete, and
    // git_commit each get a readable error and an audited refusal, never a rewrite.
    let cases: &[(&str, Value)] = &[
        // run_shell (destructive): a type-mismatched command is not coerced.
        ("run_shell", json!({ "command": 123 })),
        // apply_patch (destructive: can delete): a stringified operations array
        // that contains a delete is never parsed into a runnable patch.
        (
            "apply_patch",
            json!({ "operations": "[{\"action\":\"delete\",\"path\":\"notes.txt\"}]" }),
        ),
        // git_commit (external write: durable history): a bare-string paths list
        // is not wrapped.
        (
            "git_commit",
            json!({ "message": "m", "paths": "notes.txt" }),
        ),
    ];
    for (tool, input) in cases {
        let script = &[(*tool, "c1", input.clone())];
        let (results, events) = run_turn(script, true, RepairMode::On);
        let result = result_for(&results, "c1");
        assert!(result.is_error, "{tool}: a refused call is an error");
        assert_eq!(
            repaired_events(&events),
            0,
            "{tool}: a high-risk tool is never repaired"
        );
        assert_eq!(
            high_risk_refusals(&events),
            1,
            "{tool}: the refusal is audited"
        );
    }
}

#[test]
fn repair_off_reproduces_the_pre_repair_behaviour() {
    // Golden rollback: repair off + readable off is the raw message; repair off +
    // readable on is the Phase-1 readable error. Neither path repairs anything.
    let script = &[("git_diff", "c1", json!({ "paths": "notes.txt" }))];

    let (raw, raw_events) = run_turn(script, false, RepairMode::Off);
    assert!(result_for(&raw, "c1").output.contains("invalid input"));
    assert_eq!(repaired_events(&raw_events), 0);

    let (readable, readable_events) = run_turn(script, true, RepairMode::Off);
    assert!(result_for(&readable, "c1")
        .output
        .contains("did not match its schema"));
    assert_eq!(repaired_events(&readable_events), 0);
}

#[test]
fn a_valid_call_is_not_repaired() {
    let script = &[("read_file", "c1", json!({ "path": "notes.txt" }))];
    let (results, events) = run_turn(script, true, RepairMode::On);
    let result = result_for(&results, "c1");
    assert!(!result.is_error && result.output.contains("plumbus"));
    assert_eq!(repaired_events(&events), 0);
    assert_eq!(high_risk_refusals(&events), 0);
}
