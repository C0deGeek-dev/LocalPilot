//! Automatic completion checks preserve explicit and opaque obligations.
#![allow(clippy::unwrap_used)]

use std::{path::PathBuf, process::Command, sync::Arc};

use localpilot_config::GranularityConfig;
use localpilot_core::SessionId;
use localpilot_harness::{SessionConfig, SessionRuntime, StopReason};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Effect, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::{SessionEventKind, Store};
use localpilot_tools::{Tool, ToolContext, ToolError, ToolOutput, ToolRegistry, ToolSource};
use serde_json::json;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

fn fixture(git: bool) -> (tempfile::TempDir, SessionRuntime, Arc<FakeProvider>) {
    fixture_with(
        git,
        FakeProvider::new().tool_call("read", "read_file", json!({"path": "notes.txt"})),
        ToolRegistry::with_builtins(),
    )
}

fn fixture_with(
    git: bool,
    provider: FakeProvider,
    tools: ToolRegistry,
) -> (tempfile::TempDir, SessionRuntime, Arc<FakeProvider>) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "answer evidence\n").unwrap();
    // A detected stack whose existing verification deterministically fails.
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "invalid existing manifest !\n",
    )
    .unwrap();
    if !git {
        // A broken `.git` file stops discovery here, so a temporary directory
        // inside some other repository still has no usable baseline.
        std::fs::write(dir.path().join(".git"), "gitdir: missing-repository\n").unwrap();
    }
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
        provider
            .text("answer")
            .text("answer")
            .text("answer")
            .text("answer"),
    );
    let runtime = SessionRuntime::new(
        provider.clone(),
        tools,
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

struct ExternalMcpWrite {
    marker: PathBuf,
    fail_after_write: bool,
}

#[async_trait::async_trait]
impl Tool for ExternalMcpWrite {
    fn name(&self) -> &str {
        "external_action"
    }

    fn description(&self) -> &str {
        "Fixture MCP call with an unobserved external effect"
    }

    fn schema(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {}})
    }

    fn effects(
        &self,
        _: &serde_json::Value,
        _: &ToolContext<'_>,
    ) -> Result<Vec<Effect>, ToolError> {
        // Matches the generic MCP adapter: network permission does not describe
        // the server's actual writes or provide local touched-path evidence.
        Ok(vec![Effect::Network])
    }

    async fn invoke(
        &self,
        _: serde_json::Value,
        _: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        std::fs::write(&self.marker, "external write").unwrap();
        if self.fail_after_write {
            Err(ToolError::Failed(
                "fixture failed after external write".into(),
            ))
        } else {
            Ok(ToolOutput::ok("external action complete"))
        }
    }
}

#[tokio::test]
async fn automatic_policy_cannot_exempt_mcp_effects_outside_the_repository() {
    for fail_after_write in [false, true] {
        let external = tempfile::tempdir().unwrap();
        let marker = external.path().join("marker.txt");
        let mut tools = ToolRegistry::with_builtins();
        tools.register_from(
            Box::new(ExternalMcpWrite {
                marker: marker.clone(),
                fail_after_write,
            }),
            ToolSource::Mcp("fixture".into()),
        );
        let (dir, mut runtime, _) = fixture_with(
            true,
            FakeProvider::new().tool_call("external", "external_action", json!({})),
            tools,
        );
        runtime.set_automatic_verify_before_done();
        let (tx, _) = broadcast::channel(256);
        let stop = runtime
            .run_turn(
                "Use the external tool and answer",
                &tx,
                &CancellationToken::new(),
            )
            .await;
        assert!(marker.exists());
        assert!(Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(dir.path())
            .output()
            .unwrap()
            .stdout
            .is_empty());
        assert_eq!(
            stop,
            StopReason::NoProgress,
            "failed call: {fail_after_write}"
        );
        assert_eq!(checks(&runtime, runtime.session_id()), vec!["failed"; 3]);
    }
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

#[tokio::test]
async fn a_failed_check_on_unchanged_material_keeps_the_answer_unverified() {
    // An opaque shell command removes the read-only exemption, so the check
    // runs; it fails while repository material is unchanged. The turn ends
    // unverified with the answer kept, and no repair request follows.
    let (_dir, mut runtime, provider) = fixture_with(
        true,
        FakeProvider::new().tool_call(
            "probe",
            "run_shell",
            json!({"command": "python -c \"print(1)\""}),
        ),
        ToolRegistry::with_builtins(),
    );
    runtime.set_automatic_verify_before_done();
    let (tx, _) = broadcast::channel(256);
    assert_eq!(
        runtime
            .run_turn("Inspect and answer", &tx, &CancellationToken::new())
            .await,
        StopReason::NoProgress
    );
    assert_eq!(checks(&runtime, runtime.session_id()), vec!["failed"]);
    assert_eq!(provider.requests().len(), 2, "no repair request followed");
}

#[tokio::test]
async fn a_changed_workspace_keeps_the_repair_loop() {
    let (_dir, mut runtime, _) = fixture_with(
        true,
        FakeProvider::new().tool_call(
            "edit",
            "edit_file",
            json!({"path": "notes.txt", "old_text": "answer evidence", "new_text": "changed"}),
        ),
        ToolRegistry::with_builtins(),
    );
    runtime.set_automatic_verify_before_done();
    let (tx, _) = broadcast::channel(256);
    assert_eq!(
        runtime
            .run_turn("Edit notes and answer", &tx, &CancellationToken::new())
            .await,
        StopReason::NoProgress
    );
    assert_eq!(checks(&runtime, runtime.session_id()), vec!["failed"; 3]);
}

#[tokio::test]
async fn explicit_verification_keeps_the_repair_loop_on_an_unchanged_workspace() {
    let (_dir, mut runtime, _) = fixture_with(
        true,
        FakeProvider::new().tool_call(
            "log",
            "run_shell",
            json!({"program": "git", "args": ["log", "--oneline"]}),
        ),
        ToolRegistry::with_builtins(),
    );
    runtime.set_verify_before_done(true, None);
    let (tx, _) = broadcast::channel(256);
    assert_eq!(
        runtime
            .run_turn(
                "Read the history and answer",
                &tx,
                &CancellationToken::new()
            )
            .await,
        StopReason::NoProgress
    );
    assert_eq!(checks(&runtime, runtime.session_id()), vec!["failed"; 3]);
}

#[tokio::test]
async fn unobserved_shell_effects_never_turn_a_failed_check_into_done() {
    // Effects the repository evidence cannot see: an ignored file inside the
    // workspace and a file outside it. A failed check still never finishes.
    let external = tempfile::tempdir().unwrap();
    let outside = external.path().join("outside.txt");
    for target in ["ignored.log".to_string(), outside.display().to_string()] {
        let (dir, mut runtime, _) = fixture_with(
            true,
            FakeProvider::new().tool_call(
                "write",
                "run_shell",
                json!({"command": format!(
                    "python -c \"open(r'{}', 'w').write('x')\"",
                    target.replace('\\', "/")
                )}),
            ),
            ToolRegistry::with_builtins(),
        );
        std::fs::write(dir.path().join(".git/info/exclude"), "ignored.log\n").unwrap();
        runtime.set_automatic_verify_before_done();
        let (tx, _) = broadcast::channel(256);
        let stop = runtime
            .run_turn("Inspect and answer", &tx, &CancellationToken::new())
            .await;
        assert_ne!(stop, StopReason::Done, "{target}");
        let statuses = checks(&runtime, runtime.session_id());
        assert!(
            !statuses.is_empty() && statuses.iter().all(|s| s == "failed"),
            "{target}: {statuses:?}"
        );
    }
}
