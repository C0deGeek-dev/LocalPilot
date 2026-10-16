//! Refused review attempts are not executed implementation work. Authorized
//! error/partial writes retain their verification obligation after a downgrade.
#![allow(clippy::unwrap_used)]

use async_trait::async_trait;
use localpilot_config::GranularityConfig;
use localpilot_harness::{RuntimeEvent, SessionConfig, SessionRuntime, StopReason};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{
    Effect, Interactivity, PermissionEngine, PermissionEngineHandle, Profile, ScriptedApprover,
    Workspace,
};
use localpilot_store::{SessionEventKind, Store};
use localpilot_tools::{Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

fn runtime(
    root: &Path,
    provider: FakeProvider,
    profile: Profile,
    tools: ToolRegistry,
    optional: bool,
) -> SessionRuntime {
    SessionRuntime::new(
        Arc::new(provider),
        tools,
        PermissionEngine::new(profile, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::ephemeral(),
        Workspace::new(root).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            trusted: true,
            interactivity: Interactivity::NonInteractive,
            granularity: Some(GranularityConfig::default()),
            verify_command: Some("readonly-verification-unavailable-command".into()),
            verify_before_done: optional,
            ..SessionConfig::default()
        },
        Vec::new(),
    )
}

async fn turn(runtime: &mut SessionRuntime) -> (StopReason, Vec<RuntimeEvent>) {
    let (tx, mut rx) = broadcast::channel(256);
    let stop = runtime
        .run_turn("review the change", &tx, &CancellationToken::new())
        .await;
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    (stop, events)
}

#[tokio::test]
async fn denied_review_writes_still_spend_the_attempt_budget_without_requiring_tests() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "before\n").unwrap();
    let provider = FakeProvider::new()
        .tool_call(
            "one",
            "edit_file",
            json!({"path":"a.txt", "old_text":"before", "new_text":"after"}),
        )
        .tool_call(
            "two",
            "edit_file",
            json!({"path":"a.txt", "old_text":"before", "new_text":"wider"}),
        )
        .text("review complete");
    let mut runtime = runtime(
        dir.path(),
        provider,
        Profile::ReadOnly,
        ToolRegistry::with_builtins(),
        false,
    );
    let (stop, events) = turn(&mut runtime).await;
    assert_eq!(stop, StopReason::Done);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "before\n"
    );
    assert!(events.iter().any(|e| matches!(e, RuntimeEvent::ToolFinished { id, is_error: true, output, .. } if id == "one" && output.contains("readonly"))));
    assert!(events.iter().any(|e| matches!(e, RuntimeEvent::ToolFinished { id, is_error: true, output, .. } if id == "two" && output.contains("cumulative"))));
    assert!(!runtime
        .store()
        .read_events(runtime.session_id())
        .unwrap()
        .iter()
        .any(|e| matches!(e.kind, SessionEventKind::CheckRan { .. })));
    assert!(!events
        .iter()
        .any(|e| matches!(e, RuntimeEvent::Warning(text) if text.contains("remains unverified"))));
}

#[tokio::test]
async fn readonly_explicit_optional_check_keeps_its_unavailable_warning() {
    let dir = tempfile::tempdir().unwrap();
    let provider = FakeProvider::new()
        .tool_call(
            "write",
            "write_file",
            json!({"path":"a.txt", "content":"after"}),
        )
        .text("review complete");
    let mut runtime = runtime(
        dir.path(),
        provider,
        Profile::ReadOnly,
        ToolRegistry::with_builtins(),
        true,
    );
    let (stop, events) = turn(&mut runtime).await;
    assert_eq!(stop, StopReason::Done);
    assert!(!dir.path().join("a.txt").exists());
    assert!(events.iter().any(|e| matches!(e, RuntimeEvent::Warning(text) if text.contains("finalizing without a verify signal"))));
    assert!(runtime
        .store()
        .read_events(runtime.session_id())
        .unwrap()
        .iter()
        .any(
            |e| matches!(&e.kind, SessionEventKind::CheckRan { status, .. } if status == "denied")
        ));
}

struct WriteThenDowngrade {
    handle: Arc<Mutex<Option<PermissionEngineHandle>>>,
    partial: bool,
}

#[async_trait]
impl Tool for WriteThenDowngrade {
    fn name(&self) -> &str {
        "write_then_downgrade"
    }
    fn description(&self) -> &str {
        "test-only authorized write with a permission downgrade"
    }
    fn schema(&self) -> Value {
        json!({"type":"object"})
    }
    fn effects(&self, _: &Value, _: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(vec![Effect::WritePath {
            inside_workspace: true,
            overwrite: true,
            secret_like: false,
        }])
    }
    async fn invoke(&self, _: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        std::fs::write(ctx.workspace.root().join("a.txt"), "changed\n")
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        self.handle
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .set(PermissionEngine::new(Profile::ReadOnly, Vec::new()));
        if self.partial {
            Err(ToolError::Failed(
                "failed after writing; no touch report".into(),
            ))
        } else {
            Ok(ToolOutput::ok("write complete; no touch report"))
        }
    }
}

#[tokio::test]
async fn authorized_success_and_partial_error_remain_unverified_after_readonly_downgrade() {
    for partial in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        // No Git baseline: execution metadata must preserve the obligation even
        // though the custom tool reports no touches and may return an error.
        std::fs::write(dir.path().join("a.txt"), "before\n").unwrap();
        let handle = Arc::new(Mutex::new(None));
        let mut tools = ToolRegistry::with_builtins();
        tools.register(Box::new(WriteThenDowngrade {
            handle: Arc::clone(&handle),
            partial,
        }));
        let provider = FakeProvider::new()
            .tool_call("write", "write_then_downgrade", json!({}))
            .text("done");
        let mut runtime = runtime(dir.path(), provider, Profile::Bypass, tools, false);
        *handle.lock().unwrap() = Some(runtime.permission_engine_handle());
        let (stop, events) = turn(&mut runtime).await;
        assert_eq!(stop, StopReason::NoProgress, "partial={partial}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "changed\n"
        );
        assert!(events.iter().any(|e| matches!(e, RuntimeEvent::ToolFinished { id, is_error, .. } if id == "write" && *is_error == partial)));
        assert!(events.iter().any(|e| matches!(e, RuntimeEvent::Warning(text) if text.contains("bounded unit remains unverified"))));
        assert!(runtime.store().read_events(runtime.session_id()).unwrap().iter().any(|e| matches!(&e.kind, SessionEventKind::CheckRan { status, .. } if status == "denied")));
    }
}
