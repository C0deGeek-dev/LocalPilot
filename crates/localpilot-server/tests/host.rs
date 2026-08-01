//! Per-session host behaviour: multi-client event fanout (03.1), lock-free
//! cancel (03.2) and steer (03.3) that reach an in-flight turn without the
//! runtime mutex, and a two-client control-routed end-to-end turn (03.4).
//!
//! The cancel/steer tests need a deterministic *mid-turn* window while the
//! runtime mutex is held by `drive`. A first-party test tool provides it: it
//! blocks inside `invoke` until the test releases it, so the turn is provably
//! in flight (mutex held, `is_busy()` true) at the moment control is exercised.
//! The turn engine already races cancellation against an executing tool and
//! drains the steer queue at the safe boundary after a tool batch, so a blocked
//! tool is all the scaffolding these need.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use localpilot_core::{ContentBlock, SessionId};
use localpilot_harness::{RuntimeEvent, SessionConfig, SessionRuntime, StopReason};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{
    Effect, Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace,
};
use localpilot_server::host::{Control, ControlOutcome, SessionHost};
use localpilot_server::registry::SessionHandle;
use localpilot_store::Store;
use localpilot_tools::{Tool, ToolContext, ToolError, ToolOutput, ToolRegistry};
use serde_json::{json, Value};
use tokio::sync::{broadcast, Mutex, Notify};
use tokio::time::timeout;

/// A driven turn must reach a terminal state well within this bound; exceeding
/// it means a control call blocked on the runtime mutex (a hang the test turns
/// into a failure).
const PROMPT: Duration = Duration::from_secs(5);
/// A short bound used to prove the runtime mutex is genuinely held: a lock
/// attempt against a held mutex never completes, so it must elapse. (No false
/// pass is possible — the barrier holds the mutex until we let it go.)
const HELD: Duration = Duration::from_millis(300);

/// A first-party test tool that blocks inside `invoke` until the test releases
/// it. It signals `entered` the moment it runs — at which point the turn is
/// mid-flight and holds the runtime mutex — then parks on `release`.
struct Barrier {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl Tool for Barrier {
    fn name(&self) -> &str {
        "barrier"
    }
    fn description(&self) -> &str {
        "test-only tool that blocks mid-turn until the test releases it"
    }
    fn schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        // No effects: the Bypass engine authorizes it without a prompt.
        Ok(Vec::new())
    }
    async fn invoke(&self, _input: Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(ToolOutput::ok("released"))
    }
}

/// Build a `SessionRuntime` over a fresh temp-dir store, registering `extra`
/// tools alongside the builtins. Returns the runtime, a second store handle onto
/// the same dir (to read the persisted transcript), and the dir guard.
fn build_runtime(
    provider: FakeProvider,
    extra: Vec<Box<dyn Tool>>,
) -> (SessionRuntime, Store, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut tools = ToolRegistry::with_builtins();
    for tool in extra {
        tools.register(tool);
    }
    let runtime = SessionRuntime::new(
        Arc::new(provider),
        tools,
        PermissionEngine::new(Profile::Bypass, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(dir.path()),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            interactivity: Interactivity::NonInteractive,
            trusted: true,
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    let store = Store::open(dir.path());
    (runtime, store, dir)
}

/// Drain everything currently buffered for a subscriber, treating a lag (a slow
/// client that fell behind the ring buffer) as a resync rather than a failure.
fn drain(rx: &mut broadcast::Receiver<RuntimeEvent>) -> Vec<RuntimeEvent> {
    let mut out = Vec::new();
    loop {
        match rx.try_recv() {
            Ok(event) => out.push(event),
            Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
            Err(broadcast::error::TryRecvError::Empty)
            | Err(broadcast::error::TryRecvError::Closed) => break,
        }
    }
    out
}

fn saw_text(events: &[RuntimeEvent], needle: &str) -> bool {
    events
        .iter()
        .any(|event| matches!(event, RuntimeEvent::Text(text) if text.contains(needle)))
}

fn saw_stop(events: &[RuntimeEvent], reason: StopReason) -> bool {
    events
        .iter()
        .any(|event| matches!(event, RuntimeEvent::Stopped(r) if *r == reason))
}

/// The concatenated text of a session's persisted transcript.
fn transcript_text(store: &Store, id: SessionId) -> String {
    store
        .read_transcript(id)
        .unwrap()
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// --- 03.1 per-connection fanout ---------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_subscribers_observe_a_turn_and_a_detached_one_is_handled() {
    let provider = FakeProvider::new().text("hello there").text("second turn");
    let (runtime, _store, _dir) = build_runtime(provider, Vec::new());
    let handle: SessionHandle = Arc::new(Mutex::new(runtime));
    let host = SessionHost::new(handle).await;

    assert_eq!(host.subscriber_count(), 0, "no clients before subscribe");
    let mut rx1 = host.subscribe();
    let mut rx2 = host.subscribe();
    assert_eq!(host.subscriber_count(), 2, "attach is reflected");

    // A driven turn reaches both subscribers.
    let reason = timeout(PROMPT, host.drive("hi")).await.unwrap();
    assert_eq!(reason, StopReason::Done);
    let one = drain(&mut rx1);
    let two = drain(&mut rx2);
    assert!(
        saw_text(&one, "hello there") && saw_stop(&one, StopReason::Done),
        "rx1 observed the turn: {one:?}"
    );
    assert!(
        saw_text(&two, "hello there") && saw_stop(&two, StopReason::Done),
        "rx2 observed the turn: {two:?}"
    );

    // Detach one; the count reflects it and the driver is unaffected.
    drop(rx2);
    assert_eq!(host.subscriber_count(), 1, "detach is reflected");

    // A second turn still reaches the surviving subscriber, and driving with a
    // dropped subscriber does not error the driver.
    let reason = timeout(PROMPT, host.drive("again")).await.unwrap();
    assert_eq!(reason, StopReason::Done);
    let one = drain(&mut rx1);
    assert!(
        saw_text(&one, "second turn") && saw_stop(&one, StopReason::Done),
        "the surviving subscriber still receives: {one:?}"
    );
}

// --- 03.2 lock-free cancel ---------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_reaches_an_in_flight_turn_without_the_runtime_mutex() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let provider = FakeProvider::new().tool_call("c1", "barrier", json!({}));
    let (runtime, _store, _dir) = build_runtime(
        provider,
        vec![Box::new(Barrier {
            entered: entered.clone(),
            release: release.clone(),
        })],
    );
    let handle: SessionHandle = Arc::new(Mutex::new(runtime));
    let host = Arc::new(SessionHost::new(handle.clone()).await);

    // Start a turn; it blocks inside the barrier tool, holding the runtime mutex.
    let drive = {
        let host = host.clone();
        tokio::spawn(async move { host.drive("go").await })
    };

    // The turn is genuinely mid-flight once the barrier signals it entered.
    timeout(PROMPT, entered.notified()).await.unwrap();
    assert!(host.is_busy(), "a turn is in flight");

    // The runtime mutex is actually held by `drive`: a lock attempt cannot finish.
    assert!(
        timeout(HELD, handle.lock()).await.is_err(),
        "drive holds the runtime mutex"
    );

    // Cancel lands regardless — it reads only the turn-token slot, never the
    // runtime mutex the turn holds. (Had it needed that mutex, this would hang.)
    assert!(host.cancel(), "cancel reports a turn was in flight");
    assert!(
        host.is_busy(),
        "the turn is still in flight right after cancel"
    );

    // The turn observes the cancel promptly: the engine races cancellation
    // against the executing tool, drops the barrier future, and stops Cancelled —
    // no release needed.
    let reason = timeout(PROMPT, drive).await.unwrap().unwrap();
    assert_eq!(reason, StopReason::Cancelled);
    assert!(!host.is_busy(), "the turn-token slot is cleared on return");
}

// --- 03.3 lock-free steer ----------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn steer_reaches_an_in_flight_turn_at_a_safe_point_without_the_runtime_mutex() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    // A tool call gives an `after_tools` safe boundary; the follow-on text ends
    // the turn once the steer has been admitted.
    let provider = FakeProvider::new()
        .tool_call("c1", "barrier", json!({}))
        .text("acknowledged");
    let (runtime, store, _dir) = build_runtime(
        provider,
        vec![Box::new(Barrier {
            entered: entered.clone(),
            release: release.clone(),
        })],
    );
    let handle: SessionHandle = Arc::new(Mutex::new(runtime));
    let host = Arc::new(SessionHost::new(handle.clone()).await);
    let id = host.id();

    let drive = {
        let host = host.clone();
        tokio::spawn(async move { host.drive("go").await })
    };

    // Mid-flight, inside the barrier tool, with the runtime mutex held.
    timeout(PROMPT, entered.notified()).await.unwrap();
    assert!(host.is_busy());
    assert!(
        timeout(HELD, handle.lock()).await.is_err(),
        "drive holds the runtime mutex while we steer"
    );

    // Steer while the mutex is held: this only touches the steer queue.
    host.steer("also check the error path");

    // Release the barrier; the turn loops to its next safe boundary and admits
    // the steer before the final provider call.
    release.notify_one();
    let reason = timeout(PROMPT, drive).await.unwrap().unwrap();
    assert_eq!(reason, StopReason::Done);

    // The steered text was injected as a user message in the persisted transcript,
    // and the turn ran to completion after it.
    let text = transcript_text(&store, id);
    assert!(
        text.contains("also check the error path"),
        "steer injected mid-turn: {text}"
    );
    assert!(
        text.contains("acknowledged"),
        "the turn completed after the steer: {text}"
    );
}

// --- 03.4 control routing + two-client end-to-end ----------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_one_steers_while_the_other_watches_end_to_end() {
    use localpilot_server::registry::{RegistryError, SessionRegistry};

    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let dir = Arc::new(tempfile::tempdir().unwrap());

    // A registry factory that builds the barrier-equipped runtime; the host then
    // wraps the registry's own handle, exercising the full registry -> host stack.
    let factory = {
        let entered = entered.clone();
        let release = release.clone();
        let dir = dir.clone();
        move || -> Result<SessionRuntime, RegistryError> {
            let root = dir.path();
            let mut tools = ToolRegistry::with_builtins();
            tools.register(Box::new(Barrier {
                entered: entered.clone(),
                release: release.clone(),
            }));
            let workspace =
                Workspace::new(root).map_err(|err| RegistryError::Factory(err.to_string()))?;
            Ok(SessionRuntime::new(
                Arc::new(
                    FakeProvider::new()
                        .tool_call("c1", "barrier", json!({}))
                        .text("final answer"),
                ),
                tools,
                PermissionEngine::new(Profile::Bypass, Vec::new()),
                Box::new(ScriptedApprover::always()),
                Store::open(root),
                workspace,
                RecoveryEngine::new(RecoveryBudget::default()),
                SessionConfig {
                    interactivity: Interactivity::NonInteractive,
                    trusted: true,
                    ..SessionConfig::default()
                },
                Vec::new(),
            ))
        }
    };

    let registry = SessionRegistry::new();
    let id = registry.open_new(&factory).await.unwrap();
    let handle = registry.get(id).await.unwrap();
    let host = Arc::new(SessionHost::new(handle).await);

    // Two clients attach to the one host.
    let mut client_a = host.subscribe();
    let mut client_b = host.subscribe();
    assert_eq!(host.subscriber_count(), 2);

    // Drive one turn.
    let drive = {
        let host = host.clone();
        tokio::spawn(async move { host.drive("build it").await })
    };

    // Mid-flight: a Status control reports busy without blocking on the turn, and
    // client A steers through the control dispatch while client B just watches.
    timeout(PROMPT, entered.notified()).await.unwrap();
    match host.control(Control::Status) {
        ControlOutcome::Status(status) => {
            assert!(status.busy, "status reports the in-flight turn");
            assert_eq!(status.subscribers, 2);
            assert_eq!(status.id, id);
        }
        other => panic!("expected a status snapshot, got {other:?}"),
    }
    assert_eq!(
        host.control(Control::Steer("also verify the config".to_string())),
        ControlOutcome::Steered,
        "the control dispatch routes a steer"
    );

    // Release the barrier; the turn admits the steer at its safe boundary and ends.
    release.notify_one();
    let reason = timeout(PROMPT, drive).await.unwrap().unwrap();
    assert_eq!(reason, StopReason::Done);

    // Observed-vs-expected: both clients saw the same turn stream.
    let observed_a = drain(&mut client_a);
    let observed_b = drain(&mut client_b);
    for (label, observed) in [("A", &observed_a), ("B", &observed_b)] {
        assert!(
            saw_text(observed, "final answer"),
            "client {label} saw the final text: {observed:?}"
        );
        assert!(
            saw_stop(observed, StopReason::Done),
            "client {label} saw the turn stop: {observed:?}"
        );
    }

    // The steered turn's effect is visible in the persisted transcript.
    let text = transcript_text(&Store::open(dir.path()), id);
    assert!(
        text.contains("also verify the config"),
        "the mid-turn steer landed: {text}"
    );
    assert!(text.contains("final answer"), "the turn completed: {text}");

    // Control still routes after the turn: a cancel now reports nothing in flight,
    // and a status snapshot reports idle.
    assert_eq!(
        host.control(Control::Cancel),
        ControlOutcome::Cancelled(false)
    );
    match host.control(Control::Status) {
        ControlOutcome::Status(status) => assert!(!status.busy, "idle after the turn"),
        other => panic!("expected a status snapshot, got {other:?}"),
    }
}
