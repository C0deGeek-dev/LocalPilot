//! The same real completion check runs regardless of work sizing.
#![allow(clippy::unwrap_used)]

use std::{process::Command, sync::Arc};

use localpilot_config::GranularityConfig;
use localpilot_harness::{CheckStatus, SessionConfig, SessionRuntime, StopReason};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::{SessionEventKind, Store};
use localpilot_tools::ToolRegistry;
use serde_json::json;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

fn fixture(caps: Option<GranularityConfig>) -> (tempfile::TempDir, SessionRuntime) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("work");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("existing.txt"), "existing project material\n").unwrap();
    let git_config = dir.path().join("gitconfig");
    std::fs::write(
        &git_config,
        "[user]\n name = Fixture\n email = fixture@example.invalid\n[commit]\n gpgSign = false\n",
    )
    .unwrap();
    for args in [
        &["init", "-q"][..],
        &["add", "existing.txt"][..],
        &["commit", "-qm", "fixture"][..],
    ] {
        let output = Command::new("git")
            .env_clear()
            .env("HOME", dir.path())
            .env("USERPROFILE", dir.path())
            .env("GIT_CONFIG_GLOBAL", &git_config)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(
                "write",
                "write_file",
                json!({"path":"change.txt", "content":"new work\n"}),
            )
            .text("done")
            .text("done")
            .text("done")
            .text("done"),
    );
    let runtime = SessionRuntime::new(
        provider,
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Unrestricted, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::ephemeral(),
        Workspace::new(&root).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            granularity: caps,
            require_mutation_verification: Some(true),
            interactivity: Interactivity::NonInteractive,
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    (dir, runtime)
}

fn statuses(runtime: &SessionRuntime) -> Vec<String> {
    runtime
        .store()
        .read_events(runtime.session_id())
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.kind {
            SessionEventKind::CheckRan { status, .. } => Some(status),
            _ => None,
        })
        .collect()
}

async fn turn(runtime: &mut SessionRuntime) -> StopReason {
    let (events, _) = broadcast::channel(128);
    runtime
        .run_turn("write the new file", &events, &CancellationToken::new())
        .await
}

#[tokio::test]
async fn a_required_check_runs_once_with_different_caps_and_off() {
    for (caps, switch_off) in [
        (Some(GranularityConfig::default()), false),
        (
            Some(GranularityConfig {
                max_read_lines: Some(12),
                ..GranularityConfig::default()
            }),
            false,
        ),
        (
            Some(GranularityConfig {
                max_changed_lines: Some(8),
                ..GranularityConfig::default()
            }),
            false,
        ),
        (Some(GranularityConfig::default()), true),
        (None, false),
    ] {
        let (dir, mut runtime) = fixture(caps);
        if switch_off {
            runtime.set_envelope_enabled(false);
        }
        let command = if cfg!(windows) {
            "cmd /C type change.txt"
        } else {
            "test -f change.txt"
        };
        runtime.set_verify_before_done(false, Some(command.into()));
        assert_eq!(turn(&mut runtime).await, StopReason::Done);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("work/change.txt")).unwrap(),
            "new work\n"
        );
        assert!(runtime.current_turn_has_changes());
        assert_eq!(
            runtime.current_turn_verification(),
            Some(CheckStatus::Passed)
        );
        assert_eq!(statuses(&runtime), ["passed"]);
    }
}

#[tokio::test]
async fn failed_unavailable_and_missing_required_checks_do_not_pass_when_limits_are_off() {
    for (command, status, attempts) in [
        (
            Some("git not-a-real-262-check"),
            Some(CheckStatus::Failed),
            3,
        ),
        (
            Some("localpilot-262-fixture-missing-verifier"),
            Some(CheckStatus::Errored),
            1,
        ),
        (None, None, 0),
    ] {
        let (_dir, mut runtime) = fixture(None);
        runtime.set_verify_before_done(false, command.map(str::to_string));
        assert_eq!(
            turn(&mut runtime).await,
            StopReason::NoProgress,
            "{command:?}"
        );
        assert_eq!(runtime.current_turn_verification(), status);
        let observed = statuses(&runtime);
        assert_eq!(observed.len(), attempts);
        assert!(observed
            .iter()
            .all(|status| status == "failed" || status == "errored"));
    }
}
