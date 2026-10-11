//! A headless unit checkpoint needs an unchanged workspace through both
//! checks, runnable unit checks, and leaves a failed full check outstanding.
#![allow(clippy::unwrap_used)]

use std::{path::Path, process::Command, sync::Arc};

use localpilot_config::GranularityConfig;
use localpilot_harness::{CheckStatus, SessionConfig, SessionRuntime, StopReason};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::{SessionEventKind, Store};
use localpilot_tools::ToolRegistry;
use serde_json::json;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

fn python() -> bool {
    Command::new("python")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn git(root: &Path, args: &[&str]) {
    assert!(Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .unwrap()
        .success());
}

/// A repository whose verifier is `python <verifier>`; the verifier script
/// is committed with the project.
fn repository(verifier_body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("money.py"), "x = 1\n").unwrap();
    std::fs::write(root.join("verify.py"), verifier_body).unwrap();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.name", "Fixture"]);
    git(root, &["config", "user.email", "fixture@example.invalid"]);
    git(root, &["config", "core.autocrlf", "false"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "fixture"]);
    dir
}

fn runtime(
    root: &Path,
    provider: FakeProvider,
    profile: Profile,
    approver: ScriptedApprover,
    verify_before_done: bool,
) -> SessionRuntime {
    SessionRuntime::new(
        Arc::new(provider),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(profile, Vec::new()),
        Box::new(approver),
        Store::ephemeral(),
        Workspace::new(root).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            granularity: Some(GranularityConfig {
                headless_units: 2,
                ..GranularityConfig::default()
            }),
            verify_before_done,
            verify_command: Some("python verify.py".to_string()),
            ..SessionConfig::default()
        },
        Vec::new(),
    )
}

fn edit_then_answers(answers: usize) -> FakeProvider {
    let mut provider = FakeProvider::new().tool_call(
        "edit",
        "edit_file",
        json!({"path": "money.py", "old_text": "x = 1", "new_text": "x = 2"}),
    );
    for _ in 0..answers {
        provider = provider.text("unit done");
    }
    provider
}

async fn units(runtime: &mut SessionRuntime) -> StopReason {
    let (events, _rx) = broadcast::channel(1024);
    runtime
        .run_turn_in_units("Change x to 2.", &events, &CancellationToken::new())
        .await
}

const FAILS: &str = "import sys\nsys.exit(1)\n";

/// Statuses of the checks this session actually ran, in order.
fn check_statuses(runtime: &SessionRuntime) -> Vec<String> {
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

#[tokio::test]
async fn a_verifier_that_rewrites_the_unit_earns_no_checkpoint() {
    if !python() {
        return;
    }
    for verifier in [
        // Grows the changed file beyond the unit's bounds, syntax-valid.
        "import sys\nopen('money.py', 'w').write('x = 2\\n' + 'y = 0\\n' * 150)\nsys.exit(1)\n",
        // Writes another path.
        "import sys\nopen('other.py', 'w').write('z = 1\\n')\nsys.exit(1)\n",
        // Moves HEAD.
        "import subprocess, sys\nsubprocess.run(['git', 'commit', '-qam', 'by verifier'])\nsys.exit(1)\n",
    ] {
        let dir = repository(verifier);
        let mut runtime = runtime(
            dir.path(),
            edit_then_answers(6),
            Profile::Unrestricted,
            ScriptedApprover::always(),
            true,
        );
        let stop = units(&mut runtime).await;
        assert_eq!(stop, StopReason::NoProgress, "{verifier}");
        // Refused right after the full check moved the workspace: no unit
        // check ran and no second unit opened.
        assert_eq!(check_statuses(&runtime), ["failed"], "{verifier}");
    }
}

#[tokio::test]
async fn a_unit_check_that_cannot_run_stops_with_its_status() {
    if !python() {
        return;
    }
    let dir = repository(FAILS);
    // Approve the full check; deny the unit check that follows it.
    let approver = ScriptedApprover::new(vec![true, false]);
    let mut runtime = runtime(
        dir.path(),
        edit_then_answers(6),
        Profile::Default,
        approver,
        true,
    );
    let stop = units(&mut runtime).await;
    assert_eq!(stop, StopReason::NoProgress);
    assert_eq!(
        runtime.current_turn_verification(),
        Some(CheckStatus::Denied)
    );
    // The full check ran and failed; the unit check after it was denied.
    let statuses = check_statuses(&runtime);
    assert_eq!(statuses.len(), 2, "{statuses:?}");
    assert!(statuses[0].contains("failed"), "{statuses:?}");
    assert!(statuses[1].contains("denied"), "{statuses:?}");
}

#[tokio::test]
async fn the_failed_full_check_stays_outstanding_after_an_unfinished_task() {
    if !python() {
        return;
    }
    for verify_before_done in [true, false] {
        let dir = repository(FAILS);
        // Unit 1 checkpoints; unit 2 answers without edits until it stops;
        // a later turn answers immediately without touching anything.
        let mut runtime = runtime(
            dir.path(),
            edit_then_answers(12),
            Profile::Unrestricted,
            ScriptedApprover::always(),
            verify_before_done,
        );
        let stop = units(&mut runtime).await;
        assert_ne!(stop, StopReason::Done);
        let statuses = check_statuses(&runtime);
        assert!(
            statuses.len() >= 2 && statuses[1].contains("passed"),
            "{statuses:?}"
        );
        let ran = statuses.len();
        let (events, _rx) = broadcast::channel(1024);
        let resumed = runtime
            .run_turn("Is anything left?", &events, &CancellationToken::new())
            .await;
        assert_ne!(
            resumed,
            StopReason::Done,
            "verify_before_done={verify_before_done}: unresolved work finished without a passing full check"
        );
        // The unchanged resumed turn still ran the full check.
        let statuses = check_statuses(&runtime);
        assert!(statuses.len() > ran, "{statuses:?}");
        assert!(
            statuses[ran..].iter().all(|s| s.contains("failed")),
            "{statuses:?}"
        );
    }
}

#[tokio::test]
async fn a_passing_full_check_finishes_after_a_checkpoint() {
    if !python() {
        return;
    }
    // Passes only once both separated lines are changed.
    let dir = repository(
        "import sys\ntext = open('money.py').read()\nsys.exit(0 if 'x = 2' in text and 'y = 2' in text else 1)\n",
    );
    std::fs::write(dir.path().join("money.py"), "x = 1\n\n\n\ny = 1\n").unwrap();
    git(dir.path(), &["commit", "-qam", "two sites"]);
    let mut runtime = runtime(
        dir.path(),
        FakeProvider::new()
            .tool_call(
                "x",
                "edit_file",
                json!({"path": "money.py", "old_text": "x = 1", "new_text": "x = 2"}),
            )
            .text("unit done")
            .tool_call(
                "y",
                "edit_file",
                json!({"path": "money.py", "old_text": "y = 1", "new_text": "y = 2"}),
            )
            .text("done")
            .text("done"),
        Profile::Unrestricted,
        ScriptedApprover::always(),
        true,
    );
    assert_eq!(units(&mut runtime).await, StopReason::Done);
    let statuses = check_statuses(&runtime);
    // Full Failed, unit check Passed (checkpoint), then full Passed.
    assert_eq!(statuses.len(), 3, "{statuses:?}");
    assert!(statuses[0].contains("failed") && statuses[1].contains("passed"));
    assert!(statuses[2].contains("passed"));
}
