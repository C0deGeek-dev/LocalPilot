//! Subject 05: every `NoProgress` stop carries a precise, diagnosable
//! `TurnEnded.detail` (`signal=…`) beside the byte-for-byte `NoProgress` tag,
//! and the same notice is the stop Warning and the appended synthetic message.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use localpilot_core::{ContentBlock, Message, Role};
use localpilot_harness::{RuntimeEvent, SessionConfig, SessionRuntime, StopReason};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{
    Effect, Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace,
};
use localpilot_store::{SessionEventKind, Store};
use localpilot_tools::{Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// A test tool with CHANGING output each call — so a fixed signature never
/// repeats as a `(signature, output)` pair (no stuck-repeat), letting the
/// novelty signal fire on its own.
struct TickTool {
    calls: AtomicUsize,
}

#[async_trait]
impl Tool for TickTool {
    fn name(&self) -> &str {
        "tick"
    }
    fn description(&self) -> &str {
        "test tool with changing output"
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "additionalProperties": true })
    }
    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(Vec::new())
    }
    async fn invoke(&self, _input: Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::ok(format!("tick {n}")))
    }
}

/// A test tool that always fails — used as the grace dispatch that must not
/// replace the stuck-repeat provenance of the tool that tripped.
struct BoomTool;

#[async_trait]
impl Tool for BoomTool {
    fn name(&self) -> &str {
        "boom"
    }
    fn description(&self) -> &str {
        "test tool that always fails"
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "additionalProperties": true })
    }
    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(Vec::new())
    }
    async fn invoke(&self, _input: Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        Err(ToolError::Failed("boom".to_string()))
    }
}

fn default_rail() -> SessionConfig {
    SessionConfig {
        interactivity: Interactivity::NonInteractive,
        trusted: true,
        ..SessionConfig::default()
    }
}

fn explicit(soft: usize, hard: usize) -> SessionConfig {
    SessionConfig {
        interactivity: Interactivity::NonInteractive,
        trusted: true,
        tool_call_budget: Some(soft),
        tool_call_budget_max: Some(hard),
        tool_budget_explicit: true,
        ..SessionConfig::default()
    }
}

fn build(
    provider: FakeProvider,
    registry: ToolRegistry,
    config: SessionConfig,
    files: &[(&str, &str)],
) -> (SessionRuntime, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    for (name, contents) in files {
        std::fs::write(dir.path().join(name), contents).unwrap();
    }
    let runtime = SessionRuntime::new(
        Arc::new(provider),
        registry,
        PermissionEngine::new(Profile::Bypass, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(dir.path()),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        config,
        Vec::new(),
    );
    (runtime, dir)
}

/// The strategy-change Warning marker (the nudge) — distinct from the stop
/// notice, which says "no forward progress" (not "not making forward progress").
const NUDGE_MARKER: &str = "not making forward progress";
/// The model-visible hint appended to a tool result on the nudge.
const HINT_MARKER: &str = "These tool calls are not making forward progress";

struct Readback {
    reason: StopReason,
    stop: String,
    detail: Option<String>,
    events: Vec<RuntimeEvent>,
    messages: Vec<Message>,
}

async fn run_and_read(
    mut runtime: SessionRuntime,
    dir: &tempfile::TempDir,
    prompt: &str,
) -> Readback {
    let (events, mut rx) = broadcast::channel(4096);
    let cancel = CancellationToken::new();
    let reason = runtime.run_turn(prompt, &events, &cancel).await;

    let mut collected = Vec::new();
    while let Ok(event) = rx.try_recv() {
        collected.push(event);
    }

    let session = runtime.session_id();
    let stored = Store::open(dir.path()).read_events(session).unwrap();
    let (stop, detail) = stored
        .iter()
        .rev()
        .find_map(|event| match &event.kind {
            SessionEventKind::TurnEnded { stop, detail } => Some((stop.clone(), detail.clone())),
            _ => None,
        })
        .expect("the turn recorded a TurnEnded event");

    let messages = Store::open(dir.path()).read_transcript(session).unwrap();

    Readback {
        reason,
        stop,
        detail,
        events: collected,
        messages,
    }
}

fn message_text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// The one shared exact-surface assertion, used by every signal path: the coarse
/// tag, the persisted detail, ALL THREE sinks (stop Warning + the persisted
/// synthetic-User notice, both exactly one and byte-for-byte identical), and the
/// execution/nudge boundaries (executed `ToolFinished`, strategy-change Warnings,
/// and model-visible hints) so the new plumbing cannot mask a regression.
fn assert_exact_surface(
    r: &Readback,
    detail: &str,
    tool_calls: usize,
    nudges: usize,
    hints: usize,
) {
    assert_eq!(r.reason, StopReason::NoProgress);
    assert_eq!(r.stop, "NoProgress");
    assert_eq!(r.detail.as_deref(), Some(detail), "the persisted detail");

    let notice = format!("no forward progress this turn ({detail}); stopping instead of spinning");

    // Sink 1: exactly one stop Warning equal to the notice.
    let stop_warnings = r
        .events
        .iter()
        .filter(|event| matches!(event, RuntimeEvent::Warning(text) if *text == notice))
        .count();
    assert_eq!(stop_warnings, 1, "exactly one stop Warning == the notice");

    // Sink 2: exactly one persisted message with that exact text that is BOTH a
    // `Role::User` message AND synthetic (metadata "no tool-call progress").
    let synthetic_notices = r
        .messages
        .iter()
        .filter(|m| {
            m.role == Role::User
                && m.is_synthetic()
                && m.metadata.synthetic.as_deref() == Some("no tool-call progress")
                && message_text(m) == notice
        })
        .count();
    assert_eq!(
        synthetic_notices, 1,
        "exactly one synthetic User stop notice == the notice"
    );

    // Execution / nudge boundaries.
    let executed = r
        .events
        .iter()
        .filter(|event| matches!(event, RuntimeEvent::ToolFinished { .. }))
        .count();
    assert_eq!(executed, tool_calls, "executed tool calls");
    let nudge_warnings = r
        .events
        .iter()
        .filter(|event| matches!(event, RuntimeEvent::Warning(text) if text.contains(NUDGE_MARKER)))
        .count();
    assert_eq!(nudge_warnings, nudges, "strategy-change Warnings");
    let hint_count = r
        .events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::ToolFinished { output, .. } => Some(output),
            _ => None,
        })
        .filter(|output| output.contains(HINT_MARKER))
        .count();
    assert_eq!(hint_count, hints, "model-visible hints");
}

#[tokio::test]
async fn stuck_repeat_stop_persists_the_exact_detail_notice_and_warning() {
    // 3 identical reads trip; the 1 grace dispatch is a fourth identical read
    // whose observation updates the count to 4 before the stop.
    let mut provider = FakeProvider::new();
    for _ in 0..10 {
        provider = provider.tool_call("c", "read_file", json!({ "path": "f.txt" }));
    }
    provider = provider.text("done");

    let (runtime, dir) = build(
        provider,
        ToolRegistry::with_builtins(),
        default_rail(),
        &[("f.txt", "x\n")],
    );
    let r = run_and_read(runtime, &dir, "spin").await;
    assert_exact_surface(
        &r,
        r#"signal=stuck_repeat tool="read_file" count=4"#,
        4,
        1,
        1,
    );
}

#[tokio::test]
async fn consecutive_failures_stop_persists_the_exact_detail() {
    let mut provider = FakeProvider::new();
    for _ in 0..20 {
        provider = provider.tool_call("c", "read_file", json!({ "path": "missing.txt" }));
    }
    provider = provider.text("gave up");

    let (runtime, dir) = build(provider, ToolRegistry::with_builtins(), default_rail(), &[]);
    let r = run_and_read(runtime, &dir, "fail").await;
    assert_exact_surface(&r, "signal=consecutive_failures count=12", 12, 0, 0);
}

#[tokio::test]
async fn novelty_decay_stop_persists_the_exact_detail() {
    // One signature (same tool + input) with changing output over a full window:
    // the stuck-repeat signal never fires, only novelty decay does.
    let mut provider = FakeProvider::new();
    for i in 0..16 {
        provider = provider.tool_call(&format!("c{i}"), "tick", json!({}));
    }
    provider = provider.text("done");

    let mut registry = ToolRegistry::with_builtins();
    registry.register(Box::new(TickTool {
        calls: AtomicUsize::new(0),
    }));
    let (runtime, dir) = build(provider, registry, default_rail(), &[]);
    let r = run_and_read(runtime, &dir, "tick").await;
    assert_exact_surface(&r, "signal=novelty_decay window=12 distinct=1", 13, 1, 1);
}

#[tokio::test]
async fn failing_grace_keeps_the_trip_tool_provenance() {
    // read_file trips (X); the grace dispatch is a different tool that FAILS (Y),
    // so no successful observation replaces the signal — the stop names X/count 3,
    // never Y.
    let provider = FakeProvider::new()
        .tool_call("c1", "read_file", json!({ "path": "f.txt" }))
        .tool_call("c2", "read_file", json!({ "path": "f.txt" }))
        .tool_call("c3", "read_file", json!({ "path": "f.txt" }))
        .tool_call("c4", "boom", json!({}))
        .tool_call("c5", "read_file", json!({ "path": "f.txt" }))
        .tool_call("c6", "read_file", json!({ "path": "f.txt" }))
        .text("done");

    let mut registry = ToolRegistry::with_builtins();
    registry.register(Box::new(BoomTool));
    let (runtime, dir) = build(provider, registry, default_rail(), &[("f.txt", "x\n")]);
    let r = run_and_read(runtime, &dir, "grace fails").await;
    assert_exact_surface(
        &r,
        r#"signal=stuck_repeat tool="read_file" count=3"#,
        4,
        1,
        1,
    );
    assert!(
        !r.detail.as_deref().unwrap().contains("boom"),
        "the failing grace tool never appears in the provenance"
    );
}

#[tokio::test]
async fn explicit_budget_persists_the_original_stuck_repeat_cause() {
    // Explicit soft=5: read_file trips below the soft start; two novel reads
    // clear the DYNAMIC signal, but the persisted detail is the MONOTONE
    // first-since-reset cause — the original stuck-repeat, count 3.
    let provider = FakeProvider::new()
        .tool_call("c1", "read_file", json!({ "path": "f.txt" }))
        .tool_call("c2", "read_file", json!({ "path": "f.txt" }))
        .tool_call("c3", "read_file", json!({ "path": "f.txt" }))
        .tool_call("c4", "read_file", json!({ "path": "g.txt" }))
        .tool_call("c5", "read_file", json!({ "path": "h.txt" }))
        .tool_call("c6", "read_file", json!({ "path": "f.txt" }))
        .text("done");

    let (runtime, dir) = build(
        provider,
        ToolRegistry::with_builtins(),
        explicit(5, 50),
        &[("f.txt", "x\n"), ("g.txt", "y\n"), ("h.txt", "z\n")],
    );
    let r = run_and_read(runtime, &dir, "explicit").await;
    assert_exact_surface(
        &r,
        r#"signal=stuck_repeat tool="read_file" count=3"#,
        5,
        1,
        1,
    );
}
