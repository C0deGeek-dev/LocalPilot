//! Agent-mode session runtime integration tests, driven by the fake provider.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt as _;
use localpilot_core::{ContentBlock, Message};
use localpilot_harness::{RuntimeEvent, SessionConfig, SessionRuntime, StopReason};
use localpilot_llm::{
    FakeProvider, ModelEvent, ModelEventStream, ModelProvider, ModelRequest, ProviderDeclaration,
    ProviderError, QuotaInfo,
};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{
    Approver, Interactivity, PermissionEngine, PermissionRequest, Profile, ScriptedApprover,
    Workspace,
};
use localpilot_store::Store;
use localpilot_tools::{ToolOutputPresentation, ToolRegistry};
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

struct PendingApprover {
    called: Arc<AtomicBool>,
}

impl Approver for PendingApprover {
    fn approve<'a>(
        &'a self,
        _request: &'a PermissionRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        self.called.store(true, Ordering::SeqCst);
        Box::pin(std::future::pending())
    }
}

fn build(provider: FakeProvider, files: &[(&str, &str)], config: SessionConfig) -> Harness {
    build_with(provider, files, config, Profile::Default)
}

fn build_with(
    provider: FakeProvider,
    files: &[(&str, &str)],
    config: SessionConfig,
    profile: Profile,
) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    for (rel, contents) in files {
        let path = dir.path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }
    let store = Store::open(dir.path());
    let runtime = SessionRuntime::new(
        Arc::new(provider),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(profile, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(dir.path()),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        config,
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

fn build_from_arc(
    provider: Arc<FakeProvider>,
    files: &[(&str, &str)],
    config: SessionConfig,
    profile: Profile,
) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    for (rel, contents) in files {
        let path = dir.path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }
    let store = Store::open(dir.path());
    let runtime = SessionRuntime::new(
        provider,
        ToolRegistry::with_builtins(),
        PermissionEngine::new(profile, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(dir.path()),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        config,
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

fn build_from_provider(provider: Arc<dyn ModelProvider>, config: SessionConfig) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path());
    let runtime = SessionRuntime::new(
        provider,
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Default, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(dir.path()),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        config,
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

fn drain(rx: &mut broadcast::Receiver<RuntimeEvent>) -> Vec<RuntimeEvent> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        out.push(event);
    }
    out
}

fn message_text(messages: &[Message]) -> String {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_result_outputs(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result.output.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn an_unchanged_reread_is_elided_but_still_counts_as_a_read() {
    // Two identical read_file calls on an unchanged file: the second returns a
    // compact "elided" stub instead of the full body, but it still records as a
    // successful read_file — so a later overwrite still passes RequiresPriorRead
    // and the file content is never actually hidden (the stub says how to re-read).
    let big = "fn main() {}\n".repeat(400); // well over the stub size
    let provider = FakeProvider::new()
        .tool_call("r1", "read_file", json!({ "path": "src/lib.rs" }))
        .tool_call("r2", "read_file", json!({ "path": "src/lib.rs" }))
        .text("done");
    let mut h = build(
        provider,
        &[("src/lib.rs", big.as_str())],
        SessionConfig {
            elide_seen_reads: true,
            ..SessionConfig::default()
        },
    );

    let reason = h
        .runtime
        .run_turn("read the file twice", &h.events, &h.cancel)
        .await;
    assert_eq!(reason, StopReason::Done);

    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    let outputs = tool_result_outputs(&transcript);
    assert_eq!(outputs.len(), 2, "both reads produced a result");
    assert!(
        outputs[0].contains("fn main"),
        "the first read returns the full body"
    );
    assert!(
        outputs[1].contains("elided") && outputs[1].len() < outputs[0].len(),
        "the second, unchanged re-read is elided to a smaller stub: {}",
        outputs[1]
    );
    // Neither read is an error (so RequiresPriorRead and the scorecards still see
    // two successful read_file calls).
    assert!(
        !transcript
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(b, ContentBlock::ToolResult(r) if r.is_error())),
        "an elided read is not an error"
    );
}

#[tokio::test]
async fn elision_off_by_default_returns_full_content_on_a_reread() {
    let big = "data\n".repeat(400);
    let provider = FakeProvider::new()
        .tool_call("r1", "read_file", json!({ "path": "src/lib.rs" }))
        .tool_call("r2", "read_file", json!({ "path": "src/lib.rs" }))
        .text("done");
    // Default config: elide_seen_reads is false.
    let mut h = build(
        provider,
        &[("src/lib.rs", big.as_str())],
        SessionConfig::default(),
    );
    let _ = h.runtime.run_turn("read twice", &h.events, &h.cancel).await;
    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    let outputs = tool_result_outputs(&transcript);
    assert_eq!(outputs.len(), 2);
    assert_eq!(
        outputs[0], outputs[1],
        "with elision off both reads return full content"
    );
    assert!(!outputs[1].contains("elided"));
}

#[tokio::test]
async fn queued_soft_interrupts_are_admitted_labelled_and_recorded() {
    use localpilot_harness::{SoftInterrupt, SoftInterruptSource};

    // A user steer and a system notice queued before a call-free turn. Both are
    // admitted at the safe boundary and injected as user-role messages — the user
    // steer verbatim, the system notice labelled so it does not read as the user —
    // and each is recorded as a durable SoftInterruptInjected event.
    let provider = FakeProvider::new().text("acknowledged");
    let mut h = build(provider, &[], SessionConfig::default());
    let steer = h.runtime.steer_queue();
    steer.push("also check the error path");
    steer.push_interrupt(SoftInterrupt {
        content: "background job finished".to_string(),
        source: SoftInterruptSource::System,
        urgent: false,
    });

    let reason = h
        .runtime
        .run_turn("review the module", &h.events, &h.cancel)
        .await;
    assert_eq!(reason, StopReason::Done);

    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    let text = message_text(&transcript);
    assert!(
        text.contains("also check the error path"),
        "user steer injected: {text}"
    );
    assert!(
        text.contains("[system] background job finished"),
        "system notice injected and labelled: {text}"
    );

    let events = h.store.read_events(h.runtime.session_id()).unwrap();
    let injected: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            localpilot_store::SessionEventKind::SoftInterruptInjected { point, source } => {
                Some((point.clone(), source.clone()))
            }
            _ => None,
        })
        .collect();
    assert!(
        injected.iter().any(|(_, s)| s == "user"),
        "the user steer is recorded: {injected:?}"
    );
    assert!(
        injected.iter().any(|(_, s)| s == "system"),
        "the system notice is recorded: {injected:?}"
    );
}

struct InterruptibleProvider {
    declaration: ProviderDeclaration,
    calls: AtomicUsize,
    requests: Mutex<Vec<ModelRequest>>,
}

impl InterruptibleProvider {
    fn new() -> Self {
        Self {
            declaration: FakeProvider::new().declaration().clone(),
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl ModelProvider for InterruptibleProvider {
    fn declaration(&self) -> &ProviderDeclaration {
        &self.declaration
    }

    async fn stream(&self, request: ModelRequest) -> Result<ModelEventStream, ProviderError> {
        self.requests.lock().unwrap().push(request);
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            Ok(Box::pin(
                futures::stream::iter([Ok(ModelEvent::TextDelta("partial".to_string()))])
                    .chain(futures::stream::pending()),
            ))
        } else {
            Ok(Box::pin(futures::stream::iter([
                Ok(ModelEvent::TextDelta("after steering".to_string())),
                Ok(ModelEvent::Done),
            ])))
        }
    }
}

#[tokio::test]
async fn urgent_user_steering_preempts_an_open_stream_and_restarts_the_same_turn() {
    use localpilot_harness::{SoftInterrupt, SoftInterruptSource};

    let provider = Arc::new(InterruptibleProvider::new());
    let mut h = build_from_provider(provider.clone(), SessionConfig::default());
    let steer = h.runtime.steer_queue();
    let mut rx = h.events.subscribe();

    let mut observed = Vec::new();
    let reason = {
        let turn = h.runtime.run_turn("initial request", &h.events, &h.cancel);
        tokio::pin!(turn);
        loop {
            let event = tokio::select! {
                event = rx.recv() => event.expect("runtime event channel remains open"),
                reason = &mut turn => panic!("turn stopped before steering: {reason:?}"),
                () = tokio::time::sleep(Duration::from_secs(1)) => {
                    panic!("the first stream should start")
                }
            };
            let saw_partial = matches!(&event, RuntimeEvent::Text(text) if text == "partial");
            observed.push(event);
            if saw_partial {
                break;
            }
        }

        steer.push_interrupt(SoftInterrupt {
            content: "STEERING_SECRET".to_string(),
            source: SoftInterruptSource::User,
            urgent: true,
        });
        tokio::time::timeout(Duration::from_secs(1), &mut turn)
            .await
            .expect("urgent steering must wake the pending stream")
    };
    assert_eq!(reason, StopReason::Done);
    observed.extend(drain(&mut rx));

    let injected = observed
        .iter()
        .position(|event| {
            matches!(
                event,
                RuntimeEvent::SoftInterruptInjected { point, source }
                    if point == "during_stream" && source == "user"
            )
        })
        .expect("the UI receives a content-free steering boundary event");
    let resumed = observed
        .iter()
        .position(|event| matches!(event, RuntimeEvent::Text(text) if text == "after steering"))
        .expect("the restarted provider stream produces its final answer");
    assert!(injected < resumed);
    assert!(!format!("{:?}", observed[injected]).contains("STEERING_SECRET"));

    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(!message_text(&requests[0].messages).contains("STEERING_SECRET"));
    assert!(message_text(&requests[1].messages).contains("STEERING_SECRET"));
    drop(requests);

    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    let text = message_text(&transcript);
    assert!(!text.contains("partial"));
    assert!(text.contains("STEERING_SECRET"));
    assert!(text.contains("after steering"));
}

/// A provider that spins on identical `read_file` calls and, on a chosen call
/// index, pushes one soft interrupt of a chosen source into the runtime's steer
/// queue (as a deterministic side effect of `stream`, so no async race). Used to
/// exercise the user-steering reset of the progress breakers mid-turn.
struct SteerAfterNProvider {
    declaration: ProviderDeclaration,
    calls: AtomicUsize,
    steer_at: usize,
    source: localpilot_harness::SoftInterruptSource,
    total: usize,
    queue: Mutex<Option<localpilot_harness::SteerQueue>>,
}

impl SteerAfterNProvider {
    fn new(steer_at: usize, source: localpilot_harness::SoftInterruptSource, total: usize) -> Self {
        Self {
            declaration: FakeProvider::new().declaration().clone(),
            calls: AtomicUsize::new(0),
            steer_at,
            source,
            total,
            queue: Mutex::new(None),
        }
    }

    fn set_queue(&self, queue: localpilot_harness::SteerQueue) {
        *self.queue.lock().unwrap() = Some(queue);
    }
}

#[async_trait::async_trait]
impl ModelProvider for SteerAfterNProvider {
    fn declaration(&self) -> &ProviderDeclaration {
        &self.declaration
    }

    async fn stream(&self, _request: ModelRequest) -> Result<ModelEventStream, ProviderError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == self.steer_at {
            if let Some(queue) = self.queue.lock().unwrap().clone() {
                queue.push_interrupt(localpilot_harness::SoftInterrupt {
                    content: "STEER".to_string(),
                    source: self.source,
                    urgent: false,
                });
            }
        }
        if n < self.total {
            Ok(Box::pin(futures::stream::iter([
                Ok(ModelEvent::ToolCall {
                    id: format!("c{n}"),
                    name: "read_file".to_string(),
                    input_json: json!({ "path": "f.txt" }),
                    provider_metadata: None,
                }),
                Ok(ModelEvent::Done),
            ])))
        } else {
            Ok(Box::pin(futures::stream::iter([
                Ok(ModelEvent::TextDelta("done".to_string())),
                Ok(ModelEvent::Done),
            ])))
        }
    }
}

fn steer_spin_config() -> SessionConfig {
    SessionConfig {
        interactivity: Interactivity::NonInteractive,
        trusted: true,
        ..SessionConfig::default()
    }
}

/// Count the strategy-change Warnings, the model-visible tool-result hints, and
/// the executed tool calls (one `ToolFinished` each) in a turn's events.
fn steer_counts(events: &[RuntimeEvent]) -> (usize, usize, usize) {
    let mut warnings = 0;
    let mut hints = 0;
    let mut tool_calls = 0;
    for event in events {
        match event {
            RuntimeEvent::Warning(text) if text.contains("not making forward progress") => {
                warnings += 1;
            }
            RuntimeEvent::ToolFinished { output, .. } => {
                tool_calls += 1;
                if output.contains("These tool calls are not making forward progress") {
                    hints += 1;
                }
            }
            _ => {}
        }
    }
    (warnings, hints, tool_calls)
}

#[tokio::test]
async fn a_user_steer_mid_spin_resets_the_progress_breakers_for_a_fresh_round() {
    use localpilot_harness::SoftInterruptSource;
    // Spin on identical reads; a User steer is pushed during the third call (the
    // call that trips the detector) and admitted at the next safe boundary, which
    // resets the progress breakers. The same repetition then takes a fresh full
    // round to re-trip: 3 calls trip, reset, 3 more re-trip, and the 7th is
    // stopped — 6 executed calls, versus the un-steered 4. The preserved per-turn
    // nudge means no second Warning/hint/grace.
    let provider = Arc::new(SteerAfterNProvider::new(2, SoftInterruptSource::User, 20));
    let mut h = build_from_provider(provider.clone(), steer_spin_config());
    std::fs::write(h._dir.path().join("f.txt"), "x\n").unwrap();
    provider.set_queue(h.runtime.steer_queue());
    let mut rx = h.events.subscribe();

    let reason = h.runtime.run_turn("spin", &h.events, &h.cancel).await;
    let (warnings, hints, tool_calls) = steer_counts(&drain(&mut rx));

    assert_eq!(reason, StopReason::NoProgress);
    assert_eq!(
        tool_calls, 6,
        "the user steer reset the detector, buying a fresh round before the re-trip"
    );
    assert_eq!(
        warnings, 1,
        "the preserved nudge never emits a second Warning"
    );
    assert_eq!(hints, 1, "no second model-visible hint after the steer");
}

#[tokio::test]
async fn a_system_steer_mid_spin_does_not_reset_the_progress_breakers() {
    use localpilot_harness::SoftInterruptSource;
    // The same script, but the mid-spin interrupt is a System source: it is
    // admitted but resets nothing, so the turn stops after the normal 4 calls
    // (3 trip + 1 grace), exactly like the un-steered spin.
    let provider = Arc::new(SteerAfterNProvider::new(2, SoftInterruptSource::System, 20));
    let mut h = build_from_provider(provider.clone(), steer_spin_config());
    std::fs::write(h._dir.path().join("f.txt"), "x\n").unwrap();
    provider.set_queue(h.runtime.steer_queue());
    let mut rx = h.events.subscribe();

    let reason = h.runtime.run_turn("spin", &h.events, &h.cancel).await;
    let (warnings, hints, tool_calls) = steer_counts(&drain(&mut rx));

    assert_eq!(reason, StopReason::NoProgress);
    assert_eq!(
        tool_calls, 4,
        "a system steer does not reset the breakers; the spin stops at the normal boundary"
    );
    assert_eq!(warnings, 1);
    assert_eq!(hints, 1);
}

#[tokio::test]
async fn loop_reads_a_file_then_produces_a_final_answer() {
    let provider = FakeProvider::new()
        .tool_call("c1", "read_file", json!({ "path": "src/lib.rs" }))
        .text("the file says hello");
    let mut h = build(
        provider,
        &[("src/lib.rs", "hello world")],
        SessionConfig::default(),
    );

    let reason = h
        .runtime
        .run_turn("read the file", &h.events, &h.cancel)
        .await;
    assert_eq!(reason, StopReason::Done);

    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    // user, assistant(tool_use), tool(result), assistant(final).
    assert_eq!(transcript.len(), 4);
}

#[tokio::test]
async fn a_repeated_identical_tool_error_injects_a_strategy_change_hint() {
    // The same failing call three times in a row: the same-error breaker appends
    // a strategy-change hint to the third tool result — before the per-tool
    // failure budget (6) is spent — so a weak model stops re-sending it.
    let provider = FakeProvider::new()
        .tool_call("c1", "read_file", json!({ "path": "missing.txt" }))
        .tool_call("c2", "read_file", json!({ "path": "missing.txt" }))
        .tool_call("c3", "read_file", json!({ "path": "missing.txt" }))
        .text("giving up");
    let mut h = build(provider, &[], SessionConfig::default());

    let reason = h.runtime.run_turn("read it", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);

    let outputs = tool_result_outputs(&h.store.read_transcript(h.runtime.session_id()).unwrap());
    assert_eq!(outputs.len(), 3, "three tool calls ran, got {outputs:?}");
    assert!(
        !outputs[0].contains("[recovery]"),
        "first failure carries no hint: {}",
        outputs[0]
    );
    assert!(
        !outputs[1].contains("[recovery]"),
        "second failure carries no hint: {}",
        outputs[1]
    );
    assert!(
        outputs[2].contains("[recovery]") && outputs[2].contains("script file"),
        "the third identical failure must carry the strategy-change hint: {}",
        outputs[2]
    );
}

#[tokio::test]
async fn failures_below_the_threshold_inject_no_hint() {
    // Two identical failures stay under the breaker's threshold, so no hint is
    // injected — the breaker must not fire on a normal retry.
    let provider = FakeProvider::new()
        .tool_call("c1", "read_file", json!({ "path": "missing.txt" }))
        .tool_call("c2", "read_file", json!({ "path": "missing.txt" }))
        .text("done");
    let mut h = build(provider, &[], SessionConfig::default());

    let reason = h.runtime.run_turn("read it", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);

    let outputs = tool_result_outputs(&h.store.read_transcript(h.runtime.session_id()).unwrap());
    assert!(
        outputs.iter().all(|o| !o.contains("[recovery]")),
        "the breaker must not fire below its threshold: {outputs:?}"
    );
}

#[tokio::test]
async fn first_request_carries_the_agent_system_prompt_once() {
    let provider = Arc::new(FakeProvider::new().text("ok"));
    let mut h = build_from_arc(
        Arc::clone(&provider),
        &[],
        SessionConfig::default(),
        Profile::Default,
    );

    let reason = h.runtime.run_turn("hello", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);

    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let messages = &requests[0].messages;
    assert_eq!(
        messages.first().map(|message| message.role),
        Some(localpilot_core::Role::System)
    );
    let system_text = messages
        .first()
        .and_then(|message| message.content.first())
        .and_then(|block| match block {
            localpilot_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap();
    assert!(system_text.contains("Available tools:"));
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.role == localpilot_core::Role::System)
            .count(),
        1
    );
}

#[tokio::test]
async fn compaction_summary_does_not_produce_two_system_messages() {
    // A small context limit forces compaction once there are two prior
    // exchanges; compaction injects a summary that must fold into the single
    // leading system block rather than going out as a second system message.
    let provider = Arc::new(FakeProvider::new().text("one").text("two").text("three"));
    // The limit sits above (system prompt + one exchange + summary) but below
    // (system prompt + all three exchanges), so by the third turn the oldest
    // exchanges are dropped and a summary is injected.
    let mut h = build_from_arc(
        Arc::clone(&provider),
        &[],
        SessionConfig {
            context_token_limit: 1_400,
            ..SessionConfig::default()
        },
        Profile::Default,
    );

    let filler = "context ".repeat(250); // ~2000 chars per prompt
    for label in ["first", "second", "third"] {
        let prompt = format!("{label} {filler}");
        let reason = h.runtime.run_turn(&prompt, &h.events, &h.cancel).await;
        assert_eq!(reason, StopReason::Done);
    }

    let requests = provider.requests();
    let last = requests.last().expect("at least one request");
    let system_messages: Vec<&str> = last
        .messages
        .iter()
        .filter(|message| message.role == localpilot_core::Role::System)
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            localpilot_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();

    // Exactly one leading system message, carrying both the agent prompt and the
    // compaction summary.
    assert_eq!(
        last.messages
            .iter()
            .filter(|message| message.role == localpilot_core::Role::System)
            .count(),
        1,
        "the request must not carry two consecutive system messages"
    );
    let combined = system_messages.join("\n");
    assert!(
        combined.contains("Available tools:"),
        "system block keeps the agent prompt"
    );
    assert!(
        combined.contains("Conversation summary for trimmed history"),
        "system block folds in the compaction summary"
    );
}

#[tokio::test]
async fn aborts_a_degenerate_output_flood_early() {
    // A punctuation flood arriving as many small deltas (real streaming shape).
    let mut script: Vec<_> = (0..300)
        .map(|_| Ok(ModelEvent::TextDelta("/".to_string())))
        .collect();
    script.push(Ok(ModelEvent::Done));
    let provider = FakeProvider::new().script(script);
    let mut h = build(provider, &[], SessionConfig::default());
    let mut rx = h.events.subscribe();

    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;
    // A degenerate turn never completes as a clean answer.
    assert_ne!(reason, StopReason::Done);

    let events = drain(&mut rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, RuntimeEvent::Warning(m) if m.contains("degenerate"))),
        "the live guard should warn about degenerate output"
    );
    let streamed: usize = events
        .iter()
        .filter_map(|e| match e {
            RuntimeEvent::Text(t) => Some(t.len()),
            _ => None,
        })
        .sum();
    assert!(
        streamed < 300,
        "the flood should be cut short, got {streamed} chars"
    );
}

#[tokio::test]
async fn degenerate_output_retries_without_tool_schemas() {
    let mut flood: Vec<_> = (0..64)
        .map(|_| Ok(ModelEvent::TextDelta("/".to_string())))
        .collect();
    flood.push(Ok(ModelEvent::Done));
    let provider = Arc::new(FakeProvider::new().script(flood).text("recovered"));
    let mut h = build_from_arc(
        Arc::clone(&provider),
        &[],
        SessionConfig::default(),
        Profile::Default,
    );

    let reason = h.runtime.run_turn("ping", &h.events, &h.cancel).await;

    assert_eq!(reason, StopReason::Done);
    let requests = provider.requests();
    assert!(!requests[0].tools.is_empty());
    assert!(requests[1].tools.is_empty());
}

#[tokio::test]
async fn output_limit_stop_discards_partial_reply() {
    let provider = FakeProvider::new().script(vec![
        Ok(ModelEvent::TextDelta("partial answer".to_string())),
        Ok(ModelEvent::OutputLimit {
            message: "provider stopped at max_tokens; output may be truncated".to_string(),
        }),
        Ok(ModelEvent::Done),
    ]);
    let mut h = build(provider, &[], SessionConfig::default());
    let mut rx = h.events.subscribe();

    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;

    assert_eq!(reason, StopReason::ProviderError);
    assert!(drain(&mut rx).iter().any(|event| matches!(
        event,
        RuntimeEvent::Warning(message) if message.contains("discarding partial response")
    )));
    let events = h.store.read_events(h.runtime.session_id()).unwrap();
    let transcript = serde_json::to_string(&localpilot_store::transcript_from_events(&events))
        .expect("transcript serializes");
    assert!(
        !transcript.contains("partial answer"),
        "output-limit text must not be persisted as a final assistant reply"
    );
}

#[tokio::test]
async fn provider_error_detail_reaches_the_durable_session_log() {
    // A mid-stream ProviderError (e.g. Gemini/Vertex rejecting the request)
    // must not collapse to the bare `StopReason::ProviderError` tag in the
    // persisted log — the provider's own message has to survive alongside it.
    let provider = FakeProvider::new().script(vec![Err(ProviderError::InvalidRequest {
        message: "Invalid value at 'contents[0].role'".to_string(),
    })]);
    let mut h = build(provider, &[], SessionConfig::default());

    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;

    assert_eq!(reason, StopReason::ProviderError);
    let events = h.store.read_events(h.runtime.session_id()).unwrap();
    let detail = events.iter().find_map(|e| match &e.kind {
        localpilot_store::SessionEventKind::TurnEnded { detail, .. } => detail.clone(),
        _ => None,
    });
    assert_eq!(
        detail.as_deref(),
        Some("invalid request: Invalid value at 'contents[0].role'")
    );
}

#[tokio::test]
async fn update_plan_tool_emits_a_plan_event() {
    let provider = FakeProvider::new()
        .tool_call(
            "p1",
            "update_plan",
            json!({ "steps": [
                { "title": "investigate", "status": "done" },
                { "title": "fix", "status": "in_progress" }
            ] }),
        )
        .text("on it");
    let mut h = build(provider, &[], SessionConfig::default());
    let mut rx = h.events.subscribe();

    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);

    let plans: Vec<_> = drain(&mut rx)
        .into_iter()
        .filter_map(|event| match event {
            RuntimeEvent::Plan(steps) => Some(steps),
            _ => None,
        })
        .collect();
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].len(), 2);
    assert_eq!(plans[0][0].status, "done");
    assert_eq!(plans[0][1].title, "fix");
}

#[tokio::test]
async fn context_usage_event_is_emitted_before_request() {
    let provider = FakeProvider::new().text("ok");
    let mut h = build(provider, &[], SessionConfig::default());
    let mut rx = h.events.subscribe();

    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);

    assert!(drain(&mut rx).iter().any(|event| matches!(
        event,
        RuntimeEvent::ContextUsage { used, limit } if *used > 0 && *limit == SessionConfig::default().context_token_limit
    )));
}

#[tokio::test]
async fn clearing_conversation_resets_future_provider_context() {
    let provider = Arc::new(
        FakeProvider::new()
            .text("first answer")
            .text("second answer"),
    );
    let mut h = build_from_arc(
        Arc::clone(&provider),
        &[],
        SessionConfig::default(),
        Profile::Default,
    );
    let session_id = h.runtime.session_id();

    let reason = h
        .runtime
        .run_turn("first prompt", &h.events, &h.cancel)
        .await;
    assert_eq!(reason, StopReason::Done);

    h.runtime.clear_conversation();
    let reason = h
        .runtime
        .run_turn("second prompt", &h.events, &h.cancel)
        .await;
    assert_eq!(reason, StopReason::Done);

    assert_eq!(h.runtime.session_id(), session_id);
    let requests = provider.requests();
    let second_request = requests.last().expect("second request is recorded");
    let text = message_text(&second_request.messages);
    assert!(text.contains("second prompt"));
    assert!(!text.contains("first prompt"));
    assert!(!text.contains("first answer"));
    assert_eq!(
        second_request
            .messages
            .iter()
            .filter(|message| message.role == localpilot_core::Role::System)
            .count(),
        1
    );
}

#[tokio::test]
async fn manual_compaction_reports_noop_when_context_is_under_limit() {
    let provider = FakeProvider::new();
    let mut h = build(provider, &[], SessionConfig::default());
    let before = h.runtime.context_usage();

    let result = h.runtime.compact_conversation().await;

    assert!(!result.compacted);
    assert_eq!(result.context_limit, before.1);
    assert_eq!(result.context_used, before.0);
}

#[tokio::test]
async fn manual_compaction_stores_a_summary_for_future_turns() {
    let provider = Arc::new(
        FakeProvider::new()
            .text("one")
            .text("two")
            .text("three")
            .text("after compaction"),
    );
    let mut h = build_from_arc(
        Arc::clone(&provider),
        &[],
        SessionConfig {
            context_token_limit: 1_400,
            ..SessionConfig::default()
        },
        Profile::Default,
    );

    let filler = "context ".repeat(250);
    for label in ["first", "second", "third"] {
        let prompt = format!("{label} {filler}");
        let reason = h.runtime.run_turn(&prompt, &h.events, &h.cancel).await;
        assert_eq!(reason, StopReason::Done);
    }

    let result = h.runtime.compact_conversation().await;
    assert!(result.compacted);
    assert!(result.context_used <= result.context_limit);

    let reason = h
        .runtime
        .run_turn("after manual compact", &h.events, &h.cancel)
        .await;
    assert_eq!(reason, StopReason::Done);

    let requests = provider.requests();
    let text = message_text(&requests.last().expect("request after compaction").messages);
    assert!(text.contains("Conversation summary for trimmed history"));
    assert!(text.contains("after manual compact"));
}

#[tokio::test]
async fn clearing_after_manual_compaction_drops_the_compaction_summary() {
    let provider = Arc::new(
        FakeProvider::new()
            .text("one")
            .text("two")
            .text("three")
            .text("after clear"),
    );
    let mut h = build_from_arc(
        Arc::clone(&provider),
        &[],
        SessionConfig {
            context_token_limit: 900,
            ..SessionConfig::default()
        },
        Profile::Default,
    );

    let filler = "context ".repeat(250);
    for label in ["first", "second", "third"] {
        let prompt = format!("{label} {filler}");
        let reason = h.runtime.run_turn(&prompt, &h.events, &h.cancel).await;
        assert_eq!(reason, StopReason::Done);
    }
    assert!(h.runtime.compact_conversation().await.compacted);

    h.runtime.clear_conversation();
    let reason = h
        .runtime
        .run_turn("after clear", &h.events, &h.cancel)
        .await;
    assert_eq!(reason, StopReason::Done);

    let requests = provider.requests();
    let text = message_text(&requests.last().expect("request after clear").messages);
    assert!(text.contains("after clear"));
    assert!(!text.contains("Conversation summary for trimmed history"));
    assert!(!text.contains("first context"));
}

#[tokio::test]
async fn manual_compaction_keeps_tool_call_and_result_pairs_together() {
    let a = "a ".repeat(400);
    let b = "b ".repeat(400);
    let c = "c ".repeat(400);
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("c1", "read_file", json!({ "path": "a.txt" }))
            .text("done one")
            .tool_call("c2", "read_file", json!({ "path": "b.txt" }))
            .text("done two")
            .tool_call("c3", "read_file", json!({ "path": "c.txt" }))
            .text("done three")
            .text("after compaction"),
    );
    let mut h = build_from_arc(
        Arc::clone(&provider),
        &[("a.txt", &a), ("b.txt", &b), ("c.txt", &c)],
        SessionConfig {
            context_token_limit: 600,
            ..SessionConfig::default()
        },
        Profile::Default,
    );

    for prompt in ["read a", "read b", "read c"] {
        let reason = h.runtime.run_turn(prompt, &h.events, &h.cancel).await;
        assert_eq!(reason, StopReason::Done);
    }

    let result = h.runtime.compact_conversation().await;
    assert!(result.compacted);

    let reason = h
        .runtime
        .run_turn("after tool compaction", &h.events, &h.cancel)
        .await;
    assert_eq!(reason, StopReason::Done);

    let requests = provider.requests();
    let messages = &requests.last().expect("request after compaction").messages;
    let call_ids: Vec<_> = messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolUse(call) => Some(call.id.clone()),
            _ => None,
        })
        .collect();
    let result_ids: Vec<_> = messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result.id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(call_ids, result_ids);
}

#[tokio::test]
async fn context_boundary_compacts_and_continues_the_turn() {
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("c1", "read_file", json!({ "path": "src/lib.rs" }))
            .text("continued after compaction"),
    );
    let mut h = build_from_arc(
        Arc::clone(&provider),
        &[("src/lib.rs", "hello")],
        SessionConfig {
            context_token_limit: 1_000,
            ..SessionConfig::default()
        },
        Profile::Default,
    );

    let prompt = format!("read the file\n{}", "large context ".repeat(2_000));
    let reason = h.runtime.run_turn(&prompt, &h.events, &h.cancel).await;

    assert_eq!(reason, StopReason::Done);
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn malformed_tool_call_is_reported_and_reprompted() {
    let provider = FakeProvider::new()
        .tool_call("", "read_file", json!({ "path": "a" }))
        .text("fixed");
    let mut h = build(provider, &[], SessionConfig::default());
    let mut rx = h.events.subscribe();

    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);

    assert!(drain(&mut rx).iter().any(|event| matches!(
        event,
        RuntimeEvent::Warning(message) if message.contains("missing tool-call id")
    )));
}

#[tokio::test(start_paused = true)]
async fn retries_a_transient_connection_failure_then_succeeds() {
    // Two connection failures, then a normal response: within the retry budget.
    let provider = FakeProvider::new().fail_open(2).text("recovered");
    let mut h = build(provider, &[], SessionConfig::default());

    let reason = h.runtime.run_turn("hi", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);
}

#[tokio::test(start_paused = true)]
async fn gives_up_after_exhausting_connection_retries() {
    // More failures than the retry budget: the turn ends as a provider error.
    let provider = FakeProvider::new().fail_open(10).text("never reached");
    let mut h = build(
        provider,
        &[],
        SessionConfig {
            max_stream_retries: 2,
            ..SessionConfig::default()
        },
    );

    let reason = h.runtime.run_turn("hi", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::ProviderError);
}

#[tokio::test]
async fn reasoning_is_emitted_as_metadata_distinct_from_text() {
    let provider = FakeProvider::new().script(vec![
        Ok(ModelEvent::ReasoningDelta("let me think".to_string())),
        Ok(ModelEvent::TextDelta("the answer".to_string())),
        Ok(ModelEvent::Done),
    ]);
    let mut h = build(provider, &[], SessionConfig::default());
    let mut rx = h.events.subscribe();

    h.runtime.run_turn("hi", &h.events, &h.cancel).await;

    let events = drain(&mut rx);
    assert!(events
        .iter()
        .any(|e| matches!(e, RuntimeEvent::Reasoning(r) if r == "let me think")));
    assert!(events
        .iter()
        .any(|e| matches!(e, RuntimeEvent::Text(t) if t == "the answer")));
}

#[tokio::test]
async fn blank_reasoning_and_leading_answer_blank_lines_are_not_persisted() {
    let provider = FakeProvider::new().script(vec![
        Ok(ModelEvent::ReasoningDelta("\n\n".to_string())),
        Ok(ModelEvent::TextDelta("\n\nThe answer".to_string())),
        Ok(ModelEvent::Done),
    ]);
    let mut h = build(provider, &[], SessionConfig::default());

    let reason = h.runtime.run_turn("hi", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);

    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    let assistant = transcript
        .iter()
        .find(|message| message.role == localpilot_core::Role::Assistant)
        .expect("assistant message is persisted");
    assert_eq!(assistant.content.len(), 1);
    assert!(matches!(
        &assistant.content[0],
        localpilot_core::ContentBlock::Text { text } if text == "The answer"
    ));
}

#[tokio::test]
async fn incomplete_stream_is_retried_and_never_persisted_as_a_finished_reply() {
    let provider = FakeProvider::new()
        .script(vec![Ok(ModelEvent::TextDelta(
            "Let me start by understanding the p".to_string(),
        ))])
        .text("The complete answer.");
    let mut h = build(provider, &[], SessionConfig::default());
    let mut rx = h.events.subscribe();

    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);

    // The truncated text is never persisted as a finished reply; the repair
    // prompt that shaped the retry *is* persisted, marked synthetic, so the
    // stored transcript equals the history the model saw.
    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    assert_eq!(transcript.len(), 3);
    assert!(
        transcript[1].is_synthetic(),
        "the repair prompt is persisted and marked synthetic"
    );
    let assistant_text = transcript[2]
        .content
        .iter()
        .find_map(|block| match block {
            localpilot_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap();
    assert_eq!(assistant_text, "The complete answer.");
    assert!(!transcript
        .iter()
        .any(|message| message.content.iter().any(|block| matches!(
            block,
            localpilot_core::ContentBlock::Text { text } if text.contains("Let me start")
        ))));
    assert!(drain(&mut rx)
        .iter()
        .any(|event| matches!(event, RuntimeEvent::Recovery { .. })));
}

#[tokio::test]
async fn mid_stream_quota_error_stops_as_provider_error_and_emits_pause() {
    let quota = QuotaInfo {
        retry_after: Some(Duration::from_secs(45)),
        retryable: true,
        raw_provider_code: Some("rate_limit_exceeded".to_string()),
        ..QuotaInfo::default()
    };
    let provider = FakeProvider::new().script(vec![
        Ok(ModelEvent::TextDelta("partial answer".to_string())),
        Err(ProviderError::RateLimit { quota }),
    ]);
    let mut h = build(provider, &[], SessionConfig::default());
    let mut rx = h.events.subscribe();

    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;

    assert_eq!(reason, StopReason::ProviderError);
    assert!(drain(&mut rx)
        .iter()
        .any(|event| matches!(event, RuntimeEvent::QuotaPaused { reset } if reset.contains("45"))));
    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    assert_eq!(
        transcript.len(),
        1,
        "partial assistant text is not persisted"
    );
}

#[tokio::test]
async fn stream_decode_errors_still_use_bad_output_recovery() {
    let provider = Arc::new(FakeProvider::new().malformed().text("recovered"));
    let mut h = build_from_arc(
        Arc::clone(&provider),
        &[],
        SessionConfig::default(),
        Profile::Default,
    );
    let mut rx = h.events.subscribe();

    let reason = h.runtime.run_turn("go", &h.events, &h.cancel).await;

    assert_eq!(reason, StopReason::Done);
    assert_eq!(provider.requests().len(), 2);
    assert!(drain(&mut rx)
        .iter()
        .any(|event| matches!(event, RuntimeEvent::Recovery { .. })));
}

#[tokio::test]
async fn a_malformed_large_write_recovers_by_writing_in_pieces() {
    // Turn 1: a large write_file whose arguments fail to parse — the failure the
    // local model hits on an oversized write. Turns 2-3: given the chunk
    // instruction, the model writes the file in pieces. Turn 4: a final reply.
    let provider = Arc::new(
        FakeProvider::new()
            .script(vec![Err(ProviderError::MalformedToolArguments {
                tool: "write_file".to_string(),
                bytes: 40_000,
                reason: "expected `,` or `}`".to_string(),
            })])
            .tool_call(
                "c1",
                "write_file",
                json!({ "path": "doc.md", "content": "# Part 1\n" }),
            )
            .tool_call(
                "c2",
                "append_file",
                json!({ "path": "doc.md", "content": "# Part 2\n" }),
            )
            .text("done"),
    );
    let mut h = build_from_arc(
        Arc::clone(&provider),
        &[],
        SessionConfig::default(),
        Profile::Bypass,
    );

    let reason = h
        .runtime
        .run_turn("write the report", &h.events, &h.cancel)
        .await;

    // The recovery completes the write instead of degrading.
    assert_eq!(reason, StopReason::Done);
    assert_eq!(provider.requests().len(), 4);
    assert_eq!(
        std::fs::read_to_string(h._dir.path().join("doc.md")).unwrap(),
        "# Part 1\n# Part 2\n"
    );

    // The targeted chunk-instruction prompt (not the generic one) was injected —
    // only chosen when the chunked-write rung fires for a failed write tool.
    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    let synthetic_text: String = transcript
        .iter()
        .filter(|m| m.is_synthetic())
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            localpilot_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        synthetic_text.contains("append_file") && synthetic_text.contains("smaller pieces"),
        "expected the chunked-write repair prompt, got: {synthetic_text}"
    );
}

#[tokio::test]
async fn a_repeated_bad_turn_consumes_the_input_shrink_action() {
    // The recovery ladder emits input-shrink actions on the second consecutive
    // bad turn. The recovery path consumes them by compacting history — with a
    // large prior exchange (above the force-compaction floor but under the
    // context limit, so normal request shaping stays quiet), the only path to a
    // Compacted audit event is that consumption.
    let big = format!("project context: {}", "context ".repeat(5_000));
    let provider = Arc::new(
        FakeProvider::new()
            .text("noted") // turn 1: build large history
            .malformed() // turn 2: first bad turn
            .malformed() // turn 3: second bad turn -> input-shrink -> compaction
            .text("recovered"), // turn 4: clean
    );
    let mut h = build_from_arc(
        Arc::clone(&provider),
        &[],
        SessionConfig::default(),
        Profile::Default,
    );

    assert_eq!(
        h.runtime.run_turn(&big, &h.events, &h.cancel).await,
        StopReason::Done
    );
    assert_eq!(
        h.runtime.run_turn("continue", &h.events, &h.cancel).await,
        StopReason::Done
    );

    let events = h.store.read_events(h.runtime.session_id()).unwrap();
    use localpilot_store::SessionEventKind as Kind;
    assert!(
        events
            .iter()
            .any(|event| matches!(event.kind, Kind::Compacted { .. })),
        "the second bad turn must consume the input-shrink action by compacting history"
    );
}

#[tokio::test]
async fn a_denied_tool_call_becomes_an_error_result_not_a_crash() {
    // A destructive shell command, non-interactive, is denied; the loop keeps
    // going and the next turn produces a final answer.
    let provider = FakeProvider::new()
        .tool_call(
            "c1",
            "run_shell",
            json!({ "program": "rm", "args": ["-rf", "x"] }),
        )
        .text("could not delete");
    let config = SessionConfig {
        interactivity: Interactivity::NonInteractive,
        ..SessionConfig::default()
    };
    let mut h = build(provider, &[], config);

    let reason = h.runtime.run_turn("delete it", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);

    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    let tool_result = transcript
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|b| match b {
            localpilot_core::ContentBlock::ToolResult(r) => Some(r.clone()),
            _ => None,
        });
    let result = tool_result.expect("a tool result was recorded");
    assert!(result.is_error());
}

#[tokio::test]
async fn transcript_is_persisted_with_redaction() {
    let secret = "sk-abcdefghijklmnopqrstuvwxyz0123";
    let provider = FakeProvider::new().text("ok");
    let mut h = build(provider, &[], SessionConfig::default());

    h.runtime
        .run_turn(&format!("my key is {secret}"), &h.events, &h.cancel)
        .await;

    let raw = std::fs::read_to_string(
        h._dir
            .path()
            .join(".localpilot")
            .join("sessions")
            .join(format!("{}.jsonl", h.runtime.session_id())),
    )
    .unwrap();
    assert!(
        !raw.contains(secret),
        "secret reached the transcript: {raw}"
    );
    assert!(raw.contains("[REDACTED]"));
}

#[tokio::test]
async fn cancellation_leaves_a_consistent_transcript() {
    let provider = FakeProvider::new().text("never reached");
    let mut h = build(provider, &[], SessionConfig::default());
    h.cancel.cancel();

    let reason = h.runtime.run_turn("hello", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Cancelled);

    // Only the complete user message is persisted; the transcript still parses.
    let transcript = h.store.read_transcript(h.runtime.session_id()).unwrap();
    assert_eq!(transcript.len(), 1);
    assert_eq!(transcript[0].role, localpilot_core::Role::User);
}

#[tokio::test]
async fn transcript_is_derivable_from_the_event_log() {
    // A representative session: a denied destructive tool call, a malformed
    // stream that triggers recovery (and a synthetic repair prompt), context
    // pressure that forces compaction, then a clean answer.
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(
                "c1",
                "run_shell",
                json!({ "program": "rm", "args": ["-rf", "x"] }),
            )
            .malformed()
            .text("recovered and done"),
    );
    let mut h = build_from_arc(
        Arc::clone(&provider),
        &[],
        SessionConfig {
            interactivity: Interactivity::NonInteractive,
            context_token_limit: 600,
            ..SessionConfig::default()
        },
        Profile::Default,
    );

    // A large prompt pushes the history over the small limit so compaction
    // runs while shaping the request.
    let prompt = format!("clean up {}", "context ".repeat(500));
    let reason = h.runtime.run_turn(&prompt, &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);

    let session = h.runtime.session_id();
    let events = h.store.read_events(session).unwrap();

    // The transcript rebuilt from events equals the stored transcript,
    // including the synthetic repair prompt.
    let rebuilt = localpilot_store::transcript_from_events(&events);
    let stored = h.store.read_transcript(session).unwrap();
    assert_eq!(rebuilt, stored);
    assert!(stored.iter().any(localpilot_core::Message::is_synthetic));

    // The log carries the full audit trail for this session's shape.
    use localpilot_store::SessionEventKind as Kind;
    let has = |predicate: &dyn Fn(&Kind) -> bool| events.iter().any(|event| predicate(&event.kind));
    assert!(has(&|kind| matches!(kind, Kind::SessionOpened { .. })));
    assert!(has(&|kind| matches!(kind, Kind::TurnStarted { .. })));
    assert!(has(&|kind| matches!(kind, Kind::ToolStarted { .. })));
    assert!(has(&|kind| matches!(
        kind,
        Kind::ToolFinished { is_error: true, .. }
    )));
    assert!(has(&|kind| matches!(kind, Kind::RecoveryDiagnostic { .. })));
    assert!(has(&|kind| matches!(kind, Kind::Compacted { .. })));
    assert!(has(&|kind| matches!(
        kind,
        Kind::Message {
            origin: localpilot_store::MessageOrigin::Synthetic { .. },
            ..
        }
    )));
    assert!(has(&|kind| matches!(kind, Kind::TurnEnded { .. })));

    // The chain is well-formed: every event descends from its predecessor.
    for pair in events.windows(2) {
        assert_eq!(pair[1].parent_id, Some(pair[0].id));
    }
}

#[tokio::test]
async fn user_shell_runs_are_auditable_and_context_exclusion_works() {
    let provider = Arc::new(FakeProvider::new().text("ok"));
    let mut h = build_from_arc(
        Arc::clone(&provider),
        &[],
        SessionConfig::default(),
        Profile::Default,
    );
    let session = h.runtime.session_id();

    // An excluded run: permission-gated, in the event log, not in context.
    let excluded = h
        .runtime
        .run_user_shell("git", &["--version".to_string()], true)
        .await;
    assert!(!excluded.is_error(), "{}", excluded.output);
    assert!(h.store.read_transcript(session).unwrap().is_empty());

    // An included run lands in the transcript as a shell-role message.
    let included = h
        .runtime
        .run_user_shell("git", &["--version".to_string()], false)
        .await;
    assert!(!included.is_error(), "{}", included.output);
    let transcript = h.store.read_transcript(session).unwrap();
    assert_eq!(transcript.len(), 1);
    assert_eq!(transcript[0].role, localpilot_core::Role::UserShell);

    // Both runs are auditable from the event log, and the transcript is still
    // exactly the Message events.
    let events = h.store.read_events(session).unwrap();
    let shell_runs = events
        .iter()
        .filter(|event| {
            matches!(
                &event.kind,
                localpilot_store::SessionEventKind::ToolFinished { name, .. } if name == "run_shell"
            )
        })
        .count();
    assert_eq!(shell_runs, 2);
    assert_eq!(
        localpilot_store::transcript_from_events(&events),
        transcript
    );

    // The next model turn sees the included run but not the excluded one.
    let reason = h.runtime.run_turn("hi", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);
    let request = provider.requests().pop().unwrap();
    let roles: Vec<_> = request.messages.iter().map(|m| m.role).collect();
    assert!(roles.contains(&localpilot_core::Role::UserShell));
}

#[tokio::test]
async fn cancelled_user_shell_command_records_an_explicit_error_and_one_audit_pair() {
    let provider = Arc::new(FakeProvider::new().text("ok"));
    let mut h = build_from_arc(provider, &[], SessionConfig::default(), Profile::Bypass);
    let session = h.runtime.session_id();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        trigger.cancel();
    });

    #[cfg(windows)]
    let command = "ping.exe -n 3 127.0.0.1 | Out-Null; Write-Output LATE";
    #[cfg(not(windows))]
    let command = "sleep 2; printf LATE";
    let result = h
        .runtime
        .run_user_shell_command(command, &cancel, false)
        .await;
    assert!(result.is_error());
    assert!(result.output.contains("cancelled"), "{}", result.output);

    let transcript = h.store.read_transcript(session).unwrap();
    assert_eq!(transcript.len(), 1);
    assert_eq!(transcript[0].role, localpilot_core::Role::UserShell);
    assert!(message_text(&transcript).contains("cancelled"));

    let events = h.store.read_events(session).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                &event.kind,
                localpilot_store::SessionEventKind::ToolStarted { name, .. }
                    if name == "run_shell"
            ))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                &event.kind,
                localpilot_store::SessionEventKind::ToolFinished {
                    name,
                    is_error: true,
                    ..
                } if name == "run_shell"
            ))
            .count(),
        1
    );
    assert!(events
        .iter()
        .any(|event| matches!(&event.kind, localpilot_store::SessionEventKind::Cancelled)));
    tokio::time::sleep(Duration::from_secs(3)).await;
    let transcript_text = message_text(&h.store.read_transcript(session).unwrap());
    let result_text = transcript_text
        .split_once('\n')
        .map_or(transcript_text.as_str(), |(_, result)| result);
    assert!(
        !result_text.contains("LATE"),
        "output produced after cancellation must never enter the transcript"
    );
}

#[tokio::test]
async fn detailed_user_shell_result_carries_typed_stdout_stderr_and_exit_status() {
    let provider = Arc::new(FakeProvider::new().text("ok"));
    let mut h = build_from_arc(provider, &[], SessionConfig::default(), Profile::Bypass);
    let cancel = CancellationToken::new();
    #[cfg(windows)]
    let command = "Write-Output stdout-marker; [Console]::Error.WriteLine('stderr-marker'); exit 5";
    #[cfg(not(windows))]
    let command = "printf 'stdout-marker\\n'; printf 'stderr-marker\\n' >&2; exit 5";

    let detailed = h
        .runtime
        .run_user_shell_command_detailed(command, &cancel, true)
        .await;
    assert!(detailed.result.is_error());
    let ToolOutputPresentation::Shell(shell) = detailed.presentation.expect("typed shell output");
    assert_eq!(shell.exit_code, 5);
    assert!(shell.stdout.contains("stdout-marker"));
    assert!(shell.stderr.contains("stderr-marker"));
}

#[tokio::test]
async fn cancelling_user_shell_during_approval_starts_no_process() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let protected = outside.path().join("protected.txt");
    std::fs::write(&protected, "PROTECTED_SENTINEL").unwrap();
    let store = Store::open(dir.path());
    let called = Arc::new(AtomicBool::new(false));
    let mut runtime = SessionRuntime::new(
        Arc::new(FakeProvider::new().text("unused")),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Default, Vec::new()),
        Box::new(PendingApprover {
            called: Arc::clone(&called),
        }),
        Store::open(dir.path()),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig::default(),
        Vec::new(),
    );
    let session = runtime.session_id();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let approval_called = Arc::clone(&called);
    tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !approval_called.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the permission engine should request approval");
        trigger.cancel();
    });

    #[cfg(windows)]
    let command = format!(
        "type '{}'",
        protected.display().to_string().replace('\'', "''")
    );
    #[cfg(not(windows))]
    let command = format!("cat '{}'", protected.display());
    let result = runtime
        .run_user_shell_command(&command, &cancel, false)
        .await;
    assert!(called.load(Ordering::SeqCst));
    assert!(result.is_error());
    assert!(result.output.contains("cancelled"), "{}", result.output);
    assert!(!result.output.contains("PROTECTED_SENTINEL"));

    let transcript = store.read_transcript(session).unwrap();
    assert_eq!(transcript.len(), 1);
    assert!(message_text(&transcript).contains("cancelled"));
    let events = store.read_events(session).unwrap();
    assert!(events
        .iter()
        .any(|event| matches!(&event.kind, localpilot_store::SessionEventKind::Cancelled)));
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        localpilot_store::SessionEventKind::ToolFinished { is_error: true, .. }
    )));
}

#[tokio::test]
async fn a_bounded_turn_timeout_stops_with_a_handoff() {
    // A zero timeout trips the per-turn deadline on the first loop iteration, so a
    // long or stuck turn returns a terminal state instead of hanging — the bound a
    // non-interactive caller relies on. The provider script is never reached.
    let provider = FakeProvider::new().text("this should never stream");
    let config = SessionConfig {
        turn_timeout: Some(Duration::ZERO),
        ..SessionConfig::default()
    };
    let mut h = build(provider, &[], config);

    let reason = h
        .runtime
        .run_turn("do the work", &h.events, &h.cancel)
        .await;

    assert_eq!(reason, StopReason::TimedOut);
    let handoff = h
        .runtime
        .last_turn_handoff()
        .expect("a timed-out turn leaves a handoff");
    assert_eq!(handoff.reason, StopReason::TimedOut);
    assert_eq!(handoff.tool_calls, 0);
    assert!(handoff.files_changed.is_empty());
    assert!(!handoff.memory_written);
    // The handoff renders one parseable JSON line a caller can read off stderr.
    let line = handoff.to_json_line();
    assert!(line.contains("\"stop\":\"TimedOut\""), "got: {line}");
    assert!(line.contains("\"tool_calls\":0"), "got: {line}");
}

#[tokio::test]
async fn the_handoff_reports_tool_calls_and_files_changed() {
    let provider = FakeProvider::new()
        .tool_call(
            "c1",
            "write_file",
            json!({ "path": "out.txt", "content": "hi" }),
        )
        .text("wrote it");
    let mut h = build_with(provider, &[], SessionConfig::default(), Profile::Bypass);

    let reason = h
        .runtime
        .run_turn("create out.txt", &h.events, &h.cancel)
        .await;

    assert_eq!(reason, StopReason::Done);
    let handoff = h
        .runtime
        .last_turn_handoff()
        .expect("a finished turn leaves a handoff");
    assert_eq!(handoff.reason, StopReason::Done);
    assert!(handoff.tool_calls >= 1, "the write counts as a tool call");
    assert!(
        handoff.files_changed.iter().any(|f| f == "out.txt"),
        "the written file is reported: {:?}",
        handoff.files_changed
    );
    assert!(
        !handoff.memory_written,
        "the run-turn path never writes memory"
    );
}

// --- outcome-aware guards (#46): reported failures are not malfunctions ------

/// A `run_shell` input that completes and exits non-zero on every platform.
fn failing_command() -> serde_json::Value {
    json!({ "command": "exit 7" })
}

#[tokio::test]
async fn failing_commands_never_trip_the_stuck_guard() {
    // Twenty consecutive failing commands: every call spawned, ran, and
    // captured output, so the per-tool stuck guard must stay silent. The stop
    // that fires is the whole-turn unproductive limit (12), not `ToolStuck`.
    let mut provider = FakeProvider::new();
    for i in 0..20 {
        provider = provider.tool_call(&format!("c{i}"), "run_shell", failing_command());
    }
    let provider = provider.text("giving up");
    let mut h = build_with(provider, &[], SessionConfig::default(), Profile::Bypass);
    let mut rx = h.events.subscribe();

    let reason = h
        .runtime
        .run_turn("keep trying", &h.events, &h.cancel)
        .await;
    assert_eq!(reason, StopReason::NoProgress);

    let events = drain(&mut rx);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, RuntimeEvent::ToolStuck { .. })),
        "a healthy tool must never be reported stuck"
    );
    assert!(
        !events.iter().any(|e| matches!(
            e,
            RuntimeEvent::Warning(w) if w.contains("failed (")
        )),
        "no failure-ladder warnings for reported failures"
    );
}

#[tokio::test]
async fn a_realistic_edit_test_loop_finishes_clean() {
    // Eight repetitions of (failing command, then a successful write): the
    // canonical debugging loop. It must finish `Done` with no stuck signal.
    let mut provider = FakeProvider::new();
    for i in 0..8 {
        provider = provider
            .tool_call(&format!("f{i}"), "run_shell", failing_command())
            .tool_call(
                &format!("w{i}"),
                "write_file",
                json!({ "path": format!("fix{i}.txt"), "content": "attempt" }),
            );
    }
    let provider = provider.text("fixed it");
    let mut h = build_with(provider, &[], SessionConfig::default(), Profile::Bypass);
    let mut rx = h.events.subscribe();

    let reason = h
        .runtime
        .run_turn("fix the tests", &h.events, &h.cancel)
        .await;
    assert_eq!(reason, StopReason::Done);

    let events = drain(&mut rx);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, RuntimeEvent::ToolStuck { .. })),
        "the exact workflow the product exists for must not accuse the tool"
    );
}

#[tokio::test]
async fn a_malfunctioning_tool_still_trips_the_stuck_guard_at_six() {
    // Eight reads of a missing path are genuine malfunctions; the guard fires
    // exactly at the threshold.
    let mut provider = FakeProvider::new();
    for i in 0..8 {
        provider = provider.tool_call(
            &format!("c{i}"),
            "read_file",
            json!({ "path": "missing.txt" }),
        );
    }
    let provider = provider.text("giving up");
    let mut h = build(provider, &[], SessionConfig::default());
    let mut rx = h.events.subscribe();

    let reason = h.runtime.run_turn("read it", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);

    let events = drain(&mut rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, RuntimeEvent::ToolStuck { count: 6, .. })),
        "a genuinely unusable tool still reaches ToolStuck at 6: {events:?}"
    );
}

#[tokio::test]
async fn identical_reported_failures_get_the_failing_work_hint() {
    // Three identical failing runs with nothing landing in between: the nudge
    // fires, but with the failing-work wording — not the malfunction-shaped
    // "write it to a script file" advice.
    let provider = FakeProvider::new()
        .tool_call("c1", "run_shell", failing_command())
        .tool_call("c2", "run_shell", failing_command())
        .tool_call("c3", "run_shell", failing_command())
        .text("giving up");
    let mut h = build_with(provider, &[], SessionConfig::default(), Profile::Bypass);

    let reason = h.runtime.run_turn("run it", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);

    let outputs = tool_result_outputs(&h.store.read_transcript(h.runtime.session_id()).unwrap());
    assert_eq!(outputs.len(), 3);
    assert!(
        outputs[2].contains("reported the same failure"),
        "the third identical failure carries the failing-work hint: {}",
        outputs[2]
    );
    assert!(
        !outputs[2].contains("script file"),
        "malfunction advice is wrong for failing work: {}",
        outputs[2]
    );
}

// --- turn-handoff failure counters (#47) -------------------------------------

#[tokio::test]
async fn the_handoff_counts_both_failure_kinds_and_reads_zero_when_clean() {
    // One reported failure (a failing command) and one malfunction (a missing
    // file read) land in their own counters; a clean follow-up turn resets both.
    let provider = FakeProvider::new()
        .tool_call("c1", "run_shell", json!({ "command": "exit 7" }))
        .tool_call("c2", "read_file", json!({ "path": "missing.txt" }))
        .text("done")
        .text("clean turn");
    let mut h = build_with(provider, &[], SessionConfig::default(), Profile::Bypass);

    let reason = h.runtime.run_turn("try things", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);
    let handoff = h
        .runtime
        .last_turn_handoff()
        .expect("a handoff after a turn");
    assert_eq!(handoff.reported_failures, 1, "the failing command");
    assert_eq!(handoff.tool_failures, 1, "the missing-file read");
    assert!(handoff.stuck_tools.is_empty());
    let line = handoff.to_json_line();
    assert!(line.contains("\"reported_failures\":1"), "{line}");
    assert!(line.contains("\"tool_failures\":1"), "{line}");

    let reason = h.runtime.run_turn("say hi", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);
    let handoff = h
        .runtime
        .last_turn_handoff()
        .expect("a handoff after a turn");
    assert_eq!(handoff.reported_failures, 0);
    assert_eq!(handoff.tool_failures, 0);
}

#[tokio::test]
async fn a_stuck_tool_is_named_in_the_handoff() {
    let mut provider = FakeProvider::new();
    for i in 0..6 {
        provider = provider.tool_call(
            &format!("c{i}"),
            "read_file",
            json!({ "path": "missing.txt" }),
        );
    }
    let provider = provider.text("giving up");
    let mut h = build(provider, &[], SessionConfig::default());

    let reason = h.runtime.run_turn("read it", &h.events, &h.cancel).await;
    assert_eq!(reason, StopReason::Done);
    let handoff = h
        .runtime
        .last_turn_handoff()
        .expect("a handoff after a turn");
    assert_eq!(handoff.stuck_tools, vec!["read_file".to_string()]);
    assert!(handoff
        .to_json_line()
        .contains("\"stuck_tools\":[\"read_file\"]"));
}
