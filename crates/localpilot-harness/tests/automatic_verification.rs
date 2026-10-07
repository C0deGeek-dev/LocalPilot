//! Automatic completion checks preserve explicit and opaque obligations.
#![allow(clippy::unwrap_used)]

use std::{process::Command, sync::Arc};

use localpilot_config::GranularityConfig;
use localpilot_core::SessionId;
use localpilot_harness::{SessionConfig, SessionRuntime, StopReason};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::{SessionEventKind, Store};
use localpilot_tools::ToolRegistry;
use serde_json::json;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

fn fixture(git: bool) -> (tempfile::TempDir, SessionRuntime, Arc<FakeProvider>) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "answer evidence\n").unwrap();
    // A detected stack whose existing verification deterministically fails.
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "invalid existing manifest !\n",
    )
    .unwrap();
    if git {
        for args in [
            vec!["init", "-q"],
            vec!["add", "-A"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ],
        ] {
            assert!(Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success());
        }
    }
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("read", "read_file", json!({"path": "notes.txt"}))
            .text("answer")
            .text("answer")
            .text("answer")
            .text("answer"),
    );
    let runtime = SessionRuntime::new(
        provider.clone(),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Unrestricted, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::ephemeral(),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            granularity: Some(GranularityConfig::default()),
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    (dir, runtime, provider)
}

fn checks(runtime: &SessionRuntime, session: SessionId) -> Vec<String> {
    runtime
        .store()
        .read_events(session)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::CheckRan { status, .. } => Some(status),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn automatic_verification_skips_known_unchanged_readonly_work() {
    let (_dir, mut runtime, provider) = fixture(true);
    runtime.set_automatic_verify_before_done();
    let (tx, _) = broadcast::channel(256);
    assert_eq!(
        runtime
            .run_turn("Read notes and answer", &tx, &CancellationToken::new())
            .await,
        StopReason::Done
    );
    assert_eq!(provider.requests().len(), 2);
    assert!(checks(&runtime, runtime.session_id()).is_empty());
}

#[tokio::test]
async fn explicit_request_after_automatic_policy_still_checks_readonly_work() {
    let (_dir, mut runtime, _) = fixture(true);
    runtime.set_automatic_verify_before_done();
    runtime.set_verify_before_done(true, Some("git definitely-invalid-fixture-command".into()));
    let (tx, _) = broadcast::channel(256);
    assert_eq!(
        runtime
            .run_turn("Read notes and answer", &tx, &CancellationToken::new())
            .await,
        StopReason::NoProgress
    );
    assert_eq!(checks(&runtime, runtime.session_id()), vec!["failed"; 3]);
}

#[tokio::test]
async fn automatic_policy_cannot_exempt_work_without_a_repository_baseline() {
    let (_dir, mut runtime, _) = fixture(false);
    runtime.set_automatic_verify_before_done();
    let (tx, _) = broadcast::channel(256);
    assert_eq!(
        runtime
            .run_turn("Read notes and answer", &tx, &CancellationToken::new())
            .await,
        StopReason::NoProgress
    );
    assert_eq!(checks(&runtime, runtime.session_id()), vec!["failed"; 3]);
}
