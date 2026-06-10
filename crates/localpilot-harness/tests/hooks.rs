//! Hook-fabric behavior: notify-only observers, pre-turn context hooks,
//! tighten-only tool gates, and cancellation reaching a running tool.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use localpilot_core::ContentBlock;
use localpilot_harness::{
    ContextHook, HookEvent, RuntimeEvent, SessionConfig, SessionObserver, SessionRuntime,
    StopReason,
};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::{GateVerdict, ToolGate, ToolRegistry};
use serde_json::json;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

struct Harness {
    _dir: tempfile::TempDir,
    runtime: SessionRuntime,
    events: broadcast::Sender<RuntimeEvent>,
    cancel: CancellationToken,
    store: Store,
}

fn build(provider: FakeProvider, profile: Profile) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "contents").unwrap();
    let store = Store::open(dir.path());
    let runtime = SessionRuntime::new(
        Arc::new(provider),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(profile, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(dir.path()),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig::default(),
        Vec::new(),
    );
    let (events, _rx) = broadcast::channel(256);
    Harness {
        _dir: dir,
        runtime,
        events,
        cancel: CancellationToken::new(),
        store,
    }
}

#[derive(Default)]
struct Recorder {
    seen: Mutex<Vec<HookEvent>>,
}

impl SessionObserver for Recorder {
    fn name(&self) -> &str {
        "recorder"
    }
    fn on_event(&self, event: &HookEvent) {
        self.seen.lock().unwrap().push(event.clone());
    }
}

struct StaticContext;

impl ContextHook for StaticContext {
    fn name(&self) -> &str {
        "static-context"
    }
    fn context_for(&self, _prompt: &str) -> Option<String> {
        Some("hook-contributed project context".to_string())
    }
}

/// Blocks every `read_file` call: the tighten-only internal consumer.
struct NoReads;

impl ToolGate for NoReads {
    fn name(&self) -> &str {
        "no-reads"
    }
    fn check(
        &self,
        call: &localpilot_core::ToolCall,
        _effects: &[localpilot_sandbox::Effect],
    ) -> GateVerdict {
        if call.name == "read_file" {
            GateVerdict::Block {
                reason: "reads are disabled in this session".to_string(),
            }
        } else {
            GateVerdict::Pass
        }
    }
}

#[tokio::test]
async fn observers_see_the_turn_lifecycle() {
    let provider = FakeProvider::new()
        .tool_call("c1", "read_file", json!({ "path": "a.txt" }))
        .text("done");
    let mut h = build(provider, Profile::Default);
    let recorder = Arc::new(Recorder::default());
    h.runtime.hooks_mut().register_observer(recorder.clone());

    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);

    let seen = recorder.seen.lock().unwrap();
    assert!(seen
        .iter()
        .any(|e| matches!(e, HookEvent::TurnStarted { .. })));
    assert!(seen
        .iter()
        .any(|e| matches!(e, HookEvent::ToolStarted { name, .. } if name == "read_file")));
    assert!(seen.iter().any(|e| matches!(
        e,
        HookEvent::ToolFinished {
            is_error: false,
            ..
        }
    )));
    assert!(seen.iter().any(|e| matches!(
        e,
        HookEvent::TurnEnded {
            reason: StopReason::Done
        }
    )));
}

#[tokio::test]
async fn context_hooks_contribute_system_context_for_the_turn() {
    let provider = Arc::new(FakeProvider::new().text("ok"));
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = SessionRuntime::new(
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
    runtime
        .hooks_mut()
        .register_context_hook(Arc::new(StaticContext));
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();

    let reason = runtime.run_turn("hello", &events, &cancel).await;
    assert_eq!(reason, StopReason::Done);

    let request = provider.requests().pop().unwrap();
    let system_text: String = request
        .messages
        .iter()
        .filter(|m| m.role == localpilot_core::Role::System)
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(system_text.contains("hook-contributed project context"));
}

#[tokio::test]
async fn gates_tighten_after_the_engine_and_never_grant() {
    // The engine allows an in-workspace read; the gate still blocks it. The
    // block is a model-visible error result and the pairing contract holds.
    let provider = FakeProvider::new()
        .tool_call("c1", "read_file", json!({ "path": "a.txt" }))
        .text("blocked then done");
    let mut h = build(provider, Profile::Default);
    h.runtime.hooks_mut().register_gate(Arc::new(NoReads));

    let reason = h.runtime.run_turn("read it", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);

    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    let result = transcript
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            ContentBlock::ToolResult(r) => Some(r.clone()),
            _ => None,
        })
        .expect("a tool result was persisted");
    assert!(result.is_error);
    assert!(
        result.output.contains("blocked by no-reads"),
        "{}",
        result.output
    );
    // A gate cannot grant: a denied destructive command stays denied even
    // with a pass-everything gate registered (structural — gates only run
    // after the engine and can only block).
}

#[tokio::test]
async fn cancellation_aborts_a_running_tool_without_waiting_for_its_timeout() {
    // A long sleep through run_shell; cancellation must end the turn promptly
    // with the pairing contract intact.
    #[cfg(windows)]
    let input = json!({ "program": "ping", "args": ["-n", "30", "127.0.0.1"] });
    #[cfg(not(windows))]
    let input = json!({ "program": "sleep", "args": ["30"] });

    let provider = FakeProvider::new()
        .tool_call("c1", "run_shell", input)
        .text("never reached");
    let mut h = build(provider, Profile::Bypass);

    let cancel = h.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel.cancel();
    });

    let started = std::time::Instant::now();
    let reason = h.runtime.run_turn("wait", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Cancelled);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "cancellation must not wait out the tool: {:?}",
        started.elapsed()
    );

    // The aborted execution is recorded and paired.
    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    let calls = transcript
        .iter()
        .flat_map(|m| &m.content)
        .filter(|b| matches!(b, ContentBlock::ToolUse(_)))
        .count();
    let results: Vec<_> = transcript
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolResult(r) => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(calls, results.len());
    assert!(results[0].is_error);
    assert!(results[0].output.contains("cancelled"));

    let events = h.store.read_events(h.runtime.session_id()).unwrap();
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        localpilot_store::SessionEventKind::ToolFinished { is_error: true, .. }
    )));
    assert!(events
        .iter()
        .any(|event| matches!(&event.kind, localpilot_store::SessionEventKind::Cancelled)));
}
