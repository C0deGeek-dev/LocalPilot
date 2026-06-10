//! Session lifecycle on the event log: resume, fork, clone, and new — with
//! resume re-applying the *current* permission profile, never inheriting
//! stale elevated permissions from the resumed log.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use localpilot_core::ContentBlock;
use localpilot_harness::{SessionConfig, SessionRuntime, StopReason};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::{OpenReason, SessionEventKind, Store};
use localpilot_tools::ToolRegistry;
use serde_json::json;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

fn runtime_in(
    dir: &std::path::Path,
    provider: FakeProvider,
    profile: Profile,
    interactivity: Interactivity,
) -> SessionRuntime {
    SessionRuntime::new(
        Arc::new(provider),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(profile, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(dir),
        Workspace::new(dir).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            interactivity,
            ..SessionConfig::default()
        },
        Vec::new(),
    )
}

#[tokio::test]
async fn resume_rebuilds_the_conversation_from_the_event_log() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha").unwrap();
    let store = Store::open(dir.path());
    let (events, _rx) = broadcast::channel(256);
    let cancel = CancellationToken::new();

    // First life of the session.
    let mut first = runtime_in(
        dir.path(),
        FakeProvider::new()
            .tool_call("c1", "read_file", json!({ "path": "a.txt" }))
            .text("read it"),
        Profile::Default,
        Interactivity::Interactive,
    );
    let session = first.session_id();
    assert_eq!(
        first.run_turn("read a.txt", &events, &cancel).await,
        StopReason::Done
    );
    let original = store.read_transcript(session).unwrap();
    drop(first);

    // Second life: a fresh runtime resumes the same session and the model
    // sees the prior conversation.
    let provider = Arc::new(FakeProvider::new().text("welcome back"));
    let mut resumed = SessionRuntime::new(
        Arc::clone(&provider) as Arc<dyn localpilot_llm::ModelProvider>,
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Default, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(dir.path()),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig::default(),
        Vec::new(),
    );
    resumed.load_session(session).unwrap();
    assert_eq!(resumed.session_id(), session);
    assert_eq!(
        resumed.run_turn("continue", &events, &cancel).await,
        StopReason::Done
    );

    let request = provider.requests().pop().unwrap();
    let request_text: String = request
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(request_text.contains("read a.txt"), "history was rebuilt");
    assert!(request_text.contains("continue"));

    // The resume is itself on the audit trail, with its open reason.
    let log = store.read_events(session).unwrap();
    assert!(log.iter().any(|event| matches!(
        event.kind,
        SessionEventKind::SessionOpened {
            reason: OpenReason::Resumed
        }
    )));
    // The new transcript extends the original, never rewrites it.
    let extended = store.read_transcript(session).unwrap();
    assert_eq!(&extended[..original.len()], &original[..]);
}

#[tokio::test]
async fn resume_applies_the_current_profile_never_the_logged_one() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path());
    let (events, _rx) = broadcast::channel(256);
    let cancel = CancellationToken::new();

    // The session's first life ran under bypass: a destructive command was
    // auto-approved and executed (it fails on a missing file, but it ran).
    let mut elevated = runtime_in(
        dir.path(),
        FakeProvider::new()
            .tool_call(
                "c1",
                "run_shell",
                json!({ "program": "rm", "args": ["-rf", "ghost"] }),
            )
            .text("done"),
        Profile::Bypass,
        Interactivity::Interactive,
    );
    let session = elevated.session_id();
    let _ = elevated.run_turn("clean up", &events, &cancel).await;
    drop(elevated);

    // Resumed under the default profile, non-interactive: the same command
    // class is now denied — the log carried no permissions forward.
    let mut resumed = runtime_in(
        dir.path(),
        FakeProvider::new()
            .tool_call(
                "c2",
                "run_shell",
                json!({ "program": "rm", "args": ["-rf", "ghost"] }),
            )
            .text("could not"),
        Profile::Default,
        Interactivity::NonInteractive,
    );
    resumed.load_session(session).unwrap();
    assert_eq!(
        resumed.run_turn("again", &events, &cancel).await,
        StopReason::Done
    );

    let transcript = store.read_transcript(session).unwrap();
    let last_result = transcript
        .iter()
        .rev()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            ContentBlock::ToolResult(r) => Some(r.clone()),
            _ => None,
        })
        .unwrap();
    assert!(last_result.is_error);
    assert!(
        last_result.output.contains("permission denied"),
        "{}",
        last_result.output
    );
}

#[tokio::test]
async fn fork_branches_into_a_self_contained_session() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path());
    let (events, _rx) = broadcast::channel(256);
    let cancel = CancellationToken::new();

    let mut runtime = runtime_in(
        dir.path(),
        FakeProvider::new().text("first answer"),
        Profile::Default,
        Interactivity::Interactive,
    );
    let original = runtime.session_id();
    let _ = runtime.run_turn("start", &events, &cancel).await;
    let original_transcript = store.read_transcript(original).unwrap();

    let forked = runtime.fork_session(true).unwrap();
    assert_ne!(forked, original);
    // The fork's log is self-contained: its transcript equals the history at
    // the branch point, and it records where it branched from.
    let fork_log = store.read_events(forked).unwrap();
    assert!(fork_log.iter().any(|event| matches!(
        event.kind,
        SessionEventKind::SessionOpened {
            reason: OpenReason::Forked
        }
    )));
    assert!(fork_log
        .iter()
        .any(|event| matches!(event.kind, SessionEventKind::BranchForked { .. })));
    assert_eq!(
        localpilot_store::transcript_from_events(&fork_log),
        original_transcript
    );
    // The original log is untouched by the fork.
    assert_eq!(
        store.read_transcript(original).unwrap(),
        original_transcript
    );

    // A plain clone carries no fork marker.
    let cloned = runtime.fork_session(false).unwrap();
    let clone_log = store.read_events(cloned).unwrap();
    assert!(!clone_log
        .iter()
        .any(|event| matches!(event.kind, SessionEventKind::BranchForked { .. })));
}

#[tokio::test]
async fn a_new_session_starts_clean_on_a_fresh_chain() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path());
    let (events, _rx) = broadcast::channel(256);
    let cancel = CancellationToken::new();

    let mut runtime = runtime_in(
        dir.path(),
        FakeProvider::new().text("old").text("new"),
        Profile::Default,
        Interactivity::Interactive,
    );
    let old = runtime.session_id();
    let _ = runtime.run_turn("old prompt", &events, &cancel).await;

    runtime.start_new_session();
    let new = runtime.session_id();
    assert_ne!(new, old);
    let _ = runtime.run_turn("new prompt", &events, &cancel).await;

    let transcript = store.read_transcript(new).unwrap();
    let text: String = transcript
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(text.contains("new prompt"));
    assert!(!text.contains("old prompt"));
}
