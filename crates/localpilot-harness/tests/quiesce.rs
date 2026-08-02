//! The turn-loop graceful-shutdown (quiesce) hook: a wind-down requested while a
//! turn runs stops it at the next safe boundary, leaves a wait-like tool a
//! resumable non-error result, answers every other pending tool_use so the wire
//! pairing stays valid, and flushes the session — all without discarding the way
//! a cancel does.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use localpilot_core::{ContentBlock, Message, ToolResult};
use localpilot_harness::{QuiesceSignal, SessionConfig, SessionRuntime, StopReason};
use localpilot_llm::{FakeProvider, ModelEvent};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Effect, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::{Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// A test tool that raises a graceful-shutdown request the moment it is invoked,
/// then returns cleanly. It is how a test makes quiesce fire *mid-turn*,
/// deterministically, at a known point in the batch — no sleeps, no races.
struct QuiesceOnCall {
    signal: Arc<Mutex<Option<QuiesceSignal>>>,
}

#[async_trait]
impl Tool for QuiesceOnCall {
    fn name(&self) -> &str {
        "quiesce_now"
    }
    fn description(&self) -> &str {
        "test-only: request a graceful shutdown"
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(Vec::new())
    }
    async fn invoke(&self, _input: Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        if let Some(signal) = self.signal.lock().unwrap().as_ref() {
            signal.request();
        }
        Ok(ToolOutput::ok("requested"))
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    runtime: SessionRuntime,
    events: broadcast::Sender<localpilot_harness::RuntimeEvent>,
    cancel: CancellationToken,
    store: Store,
}

/// Build a runtime whose registry also carries the `quiesce_now` test tool, and
/// hand the tool a clone of the runtime's own quiesce signal so a call to it
/// winds the running turn down.
fn build(provider: FakeProvider) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "contents").unwrap();

    let shared: Arc<Mutex<Option<QuiesceSignal>>> = Arc::new(Mutex::new(None));
    let mut registry = ToolRegistry::with_builtins();
    registry.register(Box::new(QuiesceOnCall {
        signal: Arc::clone(&shared),
    }));

    let runtime = SessionRuntime::new(
        Arc::new(provider),
        registry,
        PermissionEngine::new(Profile::Bypass, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(dir.path()),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            trusted: true,
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    *shared.lock().unwrap() = Some(runtime.quiesce_signal());

    let (events, _rx) = broadcast::channel(256);
    let store = Store::open(dir.path());
    Harness {
        _dir: dir,
        runtime,
        events,
        cancel: CancellationToken::new(),
        store,
    }
}

/// A `ToolCall` stream event.
fn call(id: &str, name: &str, input: Value) -> Result<ModelEvent, localpilot_llm::ProviderError> {
    Ok(ModelEvent::ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        input_json: input,
        provider_metadata: None,
    })
}

/// Every `tool_use` id in the persisted history has exactly one `tool_result` id,
/// in order — the invariant a provider rejects a turn for violating.
fn assert_pairing(messages: &[Message]) {
    let calls: Vec<String> = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolUse(c) => Some(c.id.to_string()),
            _ => None,
        })
        .collect();
    let results: Vec<String> = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolResult(r) => Some(r.id.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(
        calls, results,
        "tool_use and tool_result must pair in order"
    );
}

/// The `tool_result` for `id`, if any.
fn result_for(messages: &[Message], id: &str) -> Option<ToolResult> {
    messages
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            ContentBlock::ToolResult(r) if r.id.to_string() == id => Some(r.clone()),
            _ => None,
        })
}

#[tokio::test]
async fn a_wind_down_before_the_turn_stops_cleanly_with_no_tool_calls() {
    // Nothing is running; the pre-stream boundary catches the request first.
    let mut h = build(FakeProvider::new().text("hello"));
    h.runtime.quiesce_signal().request();

    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;

    assert_eq!(reason, StopReason::Quiesced);
    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    assert_pairing(&transcript);
}

#[tokio::test]
async fn a_wait_like_tool_interrupted_mid_batch_gets_a_resumable_result() {
    // One assistant turn, two calls: `quiesce_now` (fires the wind-down) then a
    // wait-like `run_background`. The first runs and requests quiesce; the loop
    // reaches the second call's safe boundary, sees the request, and answers the
    // rest of the batch — the wait-like tool resumably.
    let provider = FakeProvider::new().script(vec![
        call("c0", "quiesce_now", json!({})),
        call(
            "c1",
            "run_background",
            json!({ "command": "echo hi", "wait_secs": 30 }),
        ),
        Ok(ModelEvent::Done),
    ]);
    let mut h = build(provider);

    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;

    assert_eq!(reason, StopReason::Quiesced);
    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    assert_pairing(&transcript);

    // The wait-like tool's result is non-error and carries its original input, so
    // the model can re-issue the exact call on resume.
    let c1 = result_for(&transcript, "c1").expect("run_background answered");
    assert!(!c1.is_error(), "a resumable wait result is not an error");
    assert!(
        c1.output.contains("re-issue") && c1.output.contains("run_background"),
        "the resumable result must tell the model how to resume: {}",
        c1.output
    );
    assert!(
        c1.output.contains("wait_secs"),
        "the resumable result must embed the original input: {}",
        c1.output
    );
}

#[tokio::test]
async fn a_non_wait_tool_queued_behind_the_wind_down_is_skipped_not_resumable() {
    // Same shape, but the trailing tool is a plain read — re-issuing it is not a
    // no-op, so it must be skipped with an interrupted (error) result, never a
    // resumable one.
    let provider = FakeProvider::new().script(vec![
        call("c0", "quiesce_now", json!({})),
        call("c1", "read_file", json!({ "path": "a.txt" })),
        Ok(ModelEvent::Done),
    ]);
    let mut h = build(provider);

    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;

    assert_eq!(reason, StopReason::Quiesced);
    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    assert_pairing(&transcript);

    let c1 = result_for(&transcript, "c1").expect("read_file answered");
    assert!(
        c1.is_error(),
        "a skipped non-wait tool is an interrupted result"
    );
    assert!(
        c1.output.contains("shutdown"),
        "the skipped result should say why: {}",
        c1.output
    );
}
