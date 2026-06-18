//! Pull-discovery broker integration tests (ADR-0031): the advertise lever, the
//! failure-driven trigger, the loose NL marker, and the end-to-end behaviour the
//! inline-vs-broker spike measures offline.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use localpilot_harness::{RuntimeEvent, SessionConfig, SessionRuntime};
use localpilot_llm::{FakeProvider, ModelRequest};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::{Broker, BrokerConfig, ToolLoad, ToolRegistry, ToolSearch};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// A runtime whose registry carries the broker's own tools, with the broker
/// installed and its catalog seeded from that registry.
struct BrokerHarness {
    _dir: tempfile::TempDir,
    runtime: SessionRuntime,
    broker: Broker,
    provider: Arc<FakeProvider>,
    store: Store,
    events: broadcast::Sender<RuntimeEvent>,
    cancel: CancellationToken,
}

fn build_broker_harness(provider: FakeProvider, config: BrokerConfig) -> BrokerHarness {
    build_broker_harness_with(provider, config, false)
}

fn build_broker_harness_with(
    provider: FakeProvider,
    config: BrokerConfig,
    marker_enabled: bool,
) -> BrokerHarness {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(provider);

    let broker = Broker::new(config);
    let mut registry = ToolRegistry::with_builtins();
    registry.register(Box::new(ToolSearch::new(broker.clone())));
    registry.register(Box::new(ToolLoad::new(broker.clone())));
    broker.set_catalog(registry.catalog());

    let mut runtime = SessionRuntime::new(
        provider.clone(),
        registry,
        PermissionEngine::new(Profile::Default, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(dir.path()),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            tool_marker_enabled: marker_enabled,
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    runtime.set_broker(Some(broker.clone()));

    let (events, _rx) = broadcast::channel(256);
    BrokerHarness {
        store: Store::open(dir.path()),
        _dir: dir,
        runtime,
        broker,
        provider,
        events,
        cancel: CancellationToken::new(),
    }
}

/// The tool names advertised in the first recorded request.
fn advertised(requests: &[ModelRequest]) -> Vec<String> {
    requests
        .first()
        .map(|request| request.tools.iter().map(|t| t.name.clone()).collect())
        .unwrap_or_default()
}

// --- the advertise lever (03.4) ---

#[tokio::test]
async fn the_broker_narrows_advertised_tools_to_the_working_set() {
    let mut h = build_broker_harness(FakeProvider::new().text("done"), BrokerConfig::default());
    let _ = h.runtime.run_turn("hello", &h.events, &h.cancel).await;

    let names = advertised(&h.provider.requests());
    // Core + the broker's own tools are advertised.
    assert!(
        names.contains(&"read_file".to_string()),
        "core tool: {names:?}"
    );
    assert!(names.contains(&"tool_search".to_string()), "{names:?}");
    assert!(names.contains(&"tool_load".to_string()), "{names:?}");
    // A non-core tool's schema is withheld until it is revealed.
    assert!(
        !names.contains(&"git_commit".to_string()),
        "git_commit should not be advertised before reveal: {names:?}"
    );
    assert!(
        !names.contains(&"fetch".to_string()),
        "fetch should not be advertised before reveal: {names:?}"
    );
}

#[tokio::test]
async fn revealing_a_tool_advertises_it_on_the_next_turn() {
    let mut h = build_broker_harness(FakeProvider::new().text("done"), BrokerConfig::default());

    // Reveal a non-core tool out of band (the broker tools do this in-session).
    assert!(matches!(
        h.broker.reveal("fetch"),
        localpilot_tools::RevealOutcome::Revealed { .. }
    ));

    let _ = h.runtime.run_turn("hello", &h.events, &h.cancel).await;
    let names = advertised(&h.provider.requests());
    assert!(
        names.contains(&"fetch".to_string()),
        "fetch should be advertised after reveal: {names:?}"
    );
}

/// The tool names advertised in the request at `turn` (0-based).
fn advertised_at(requests: &[ModelRequest], turn: usize) -> Vec<String> {
    requests
        .get(turn)
        .map(|request| request.tools.iter().map(|t| t.name.clone()).collect())
        .unwrap_or_default()
}

/// Drain the broadcast receiver and collect `(name, is_error, output)` for every
/// finished tool call.
fn finished_tools(rx: &mut broadcast::Receiver<RuntimeEvent>) -> Vec<(String, bool, String)> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let RuntimeEvent::ToolFinished {
            name,
            is_error,
            output,
            ..
        } = event
        {
            out.push((name, is_error, output));
        }
    }
    out
}

// --- failure-driven re-resolution (04.1–04.3) ---

#[tokio::test]
async fn a_call_to_an_unadvertised_tool_reveals_it_without_running_it() {
    let provider = FakeProvider::new()
        .tool_call("c1", "git_log", serde_json::json!({}))
        .text("done");
    let mut h = build_broker_harness(provider, BrokerConfig::default());
    let mut rx = h.events.subscribe();

    let _ = h
        .runtime
        .run_turn("show me the history", &h.events, &h.cancel)
        .await;

    let requests = h.provider.requests();
    // Turn 1 did not advertise git_log; turn 2 does, because the failed call
    // revealed it.
    assert!(
        !advertised_at(&requests, 0).contains(&"git_log".to_string()),
        "git_log should be unadvertised on turn 1"
    );
    assert!(
        advertised_at(&requests, 1).contains(&"git_log".to_string()),
        "git_log should be revealed and advertised on turn 2"
    );
    // The attempted call did not run: its result is the re-resolution redirect,
    // not git output, and it is not an error.
    let finished = finished_tools(&mut rx);
    let c1 = finished
        .iter()
        .find(|(name, _, _)| name == "git_log")
        .expect("a git_log tool result");
    assert!(
        !c1.1,
        "the re-resolution is a redirect, not an error: {c1:?}"
    );
    assert!(
        c1.2.to_lowercase().contains("retry") && c1.2.contains("git_log"),
        "expected a re-resolution message, got: {}",
        c1.2
    );
    assert!(
        h.broker.is_advertised("git_log"),
        "revealed by the failed call"
    );
}

#[tokio::test]
async fn a_retired_tool_routes_to_its_overlay_replacement() {
    let provider = FakeProvider::new()
        .tool_call("c1", "legacy_log", serde_json::json!({}))
        .text("done");
    let mut h = build_broker_harness(provider, BrokerConfig::default());
    h.broker.deprecate("legacy_log", "git_log");
    let mut rx = h.events.subscribe();

    let _ = h
        .runtime
        .run_turn("show history with the old tool", &h.events, &h.cancel)
        .await;

    let finished = finished_tools(&mut rx);
    let c1 = finished
        .iter()
        .find(|(name, _, _)| name == "legacy_log")
        .expect("a legacy_log tool result");
    assert!(
        c1.2.contains("retired") && c1.2.contains("git_log"),
        "expected a retired-tool redirect, got: {}",
        c1.2
    );
    assert!(h.broker.is_advertised("git_log"), "replacement revealed");
}

#[tokio::test]
async fn an_unresolvable_tool_is_a_clean_terminal_message() {
    let provider = FakeProvider::new()
        .tool_call("c1", "frobnicate_quux", serde_json::json!({}))
        .text("giving up");
    let mut h = build_broker_harness(provider, BrokerConfig::default());
    let mut rx = h.events.subscribe();

    let _ = h
        .runtime
        .run_turn("do the impossible", &h.events, &h.cancel)
        .await;

    let finished = finished_tools(&mut rx);
    let c1 = finished
        .iter()
        .find(|(name, _, _)| name == "frobnicate_quux")
        .expect("a tool result for the unknown call");
    assert!(
        c1.2.contains("no available tool matches"),
        "expected a terminal no-match message, got: {}",
        c1.2
    );
}

// --- loose NL marker (04.4, 04.5) ---

#[tokio::test]
async fn a_need_marker_reveals_a_tool_when_the_marker_trigger_is_on() {
    let provider = FakeProvider::new()
        .text("I should grab the page.\nNEED: fetch a web page over http")
        .text("done");
    let mut h = build_broker_harness_with(provider, BrokerConfig::default(), true);

    let _ = h
        .runtime
        .run_turn("get the page", &h.events, &h.cancel)
        .await;

    assert!(
        h.broker.is_advertised("fetch"),
        "the NEED marker should have revealed fetch"
    );
    // Turn 2 advertises the revealed tool.
    assert!(
        advertised_at(&h.provider.requests(), 1).contains(&"fetch".to_string()),
        "fetch advertised on the turn after the marker"
    );
}

#[tokio::test]
async fn prose_without_a_marker_reveals_nothing() {
    let provider = FakeProvider::new().text("I could fetch a web page but I won't.");
    let mut h = build_broker_harness_with(provider, BrokerConfig::default(), true);

    let _ = h.runtime.run_turn("ponder", &h.events, &h.cancel).await;
    assert!(
        !h.broker.is_advertised("fetch"),
        "plain prose must not trigger a reveal"
    );
}

#[tokio::test]
async fn a_need_marker_does_nothing_when_the_trigger_is_off() {
    let provider = FakeProvider::new().text("NEED: fetch a web page over http");
    let mut h = build_broker_harness(provider, BrokerConfig::default());
    // marker trigger left off (the default)

    let _ = h
        .runtime
        .run_turn("get the page", &h.events, &h.cancel)
        .await;
    assert!(
        !h.broker.is_advertised("fetch"),
        "the marker must be inert when the trigger is off"
    );
}

// --- end-to-end behaviour, the offline inline-vs-broker spike (04.6, 00.8) ---

#[tokio::test]
async fn the_broker_recovers_an_unadvertised_tool_where_the_baseline_errors() {
    // Broker on: the model calls an unadvertised tool, the failure-driven reveal
    // makes it available, and the retry dispatches it — the task completes.
    let provider = FakeProvider::new()
        .tool_call("c1", "git_log", serde_json::json!({}))
        .tool_call("c2", "git_log", serde_json::json!({}))
        .text("done");
    let mut h = build_broker_harness(provider, BrokerConfig::default());
    let mut rx = h.events.subscribe();
    let reason = h
        .runtime
        .run_turn("show history", &h.events, &h.cancel)
        .await;

    let finished = finished_tools(&mut rx);
    // Two git_log results: the first is the re-resolution redirect, the second is
    // an actual dispatch (it ran — on a non-repo it errors, but it executed).
    let git_log_results: Vec<&(String, bool, String)> =
        finished.iter().filter(|(n, _, _)| n == "git_log").collect();
    assert_eq!(git_log_results.len(), 2, "got {finished:?}");
    assert!(
        git_log_results[0].2.to_lowercase().contains("retry"),
        "first call is the redirect: {}",
        git_log_results[0].2
    );
    assert!(
        !git_log_results[1].2.to_lowercase().contains("retry"),
        "second call actually dispatched: {}",
        git_log_results[1].2
    );
    assert_eq!(reason, localpilot_harness::StopReason::Done);
}

#[tokio::test]
async fn without_the_broker_an_unknown_tool_just_errors() {
    // The baseline the broker replaces: with no broker, a call to a tool that is
    // not registered is a bare `unknown tool` error the model cannot recover from.
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("c1", "scrape_the_web", serde_json::json!({}))
            .text("stuck"),
    );
    let mut runtime = SessionRuntime::new(
        provider.clone(),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Default, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(dir.path()),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig::default(),
        Vec::new(),
    );
    let (events, _rx) = broadcast::channel(256);
    let mut rx = events.subscribe();
    let _ = runtime
        .run_turn("scrape it", &events, &CancellationToken::new())
        .await;
    let finished = finished_tools(&mut rx);
    let c1 = finished
        .iter()
        .find(|(name, _, _)| name == "scrape_the_web")
        .expect("a result for the unknown call");
    assert!(c1.1, "an unknown tool with no broker is an error");
    assert!(
        c1.2.contains("unknown tool"),
        "expected the bare unknown-tool error, got: {}",
        c1.2
    );
}

// --- telemetry + graduation (05.1, 05.3, 05.4) ---

fn resolution_events(store: &Store, id: localpilot_core::SessionId) -> usize {
    store
        .read_events(id)
        .unwrap()
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                localpilot_store::SessionEventKind::ToolResolution { .. }
            )
        })
        .count()
}

#[tokio::test]
async fn with_learning_on_a_resolution_is_recorded_to_the_event_log() {
    let provider = FakeProvider::new()
        .tool_call("c1", "git_log", serde_json::json!({}))
        .text("done");
    let mut h = build_broker_harness(
        provider,
        BrokerConfig {
            learning_enabled: true,
            ..BrokerConfig::default()
        },
    );
    let _ = h.runtime.run_turn("history", &h.events, &h.cancel).await;
    assert_eq!(
        resolution_events(&h.store, h.runtime.session_id()),
        1,
        "a failure-driven resolution should be recorded once"
    );
}

#[tokio::test]
async fn with_learning_off_no_resolution_telemetry_is_written() {
    let provider = FakeProvider::new()
        .tool_call("c1", "git_log", serde_json::json!({}))
        .text("done");
    let mut h = build_broker_harness(provider, BrokerConfig::default()); // learning off
    let _ = h.runtime.run_turn("history", &h.events, &h.cancel).await;
    // The broker still works (git_log was revealed) but nothing was learned.
    assert!(
        h.broker.is_advertised("git_log"),
        "mechanical freshness still works"
    );
    assert_eq!(
        resolution_events(&h.store, h.runtime.session_id()),
        0,
        "no telemetry with learning off"
    );
}

#[tokio::test]
async fn a_hot_tool_graduates_across_repeated_reveals() {
    // With learning on and a low threshold, repeated failure-driven reveals of the
    // same tool graduate it into the always-advertised set.
    let provider = FakeProvider::new()
        .tool_call("c1", "git_log", serde_json::json!({}))
        .tool_call("c2", "git_log", serde_json::json!({}))
        .text("done");
    let mut h = build_broker_harness(
        provider,
        BrokerConfig {
            learning_enabled: true,
            graduation_threshold: 2,
            ..BrokerConfig::default()
        },
    );
    // First reveal happens via the failed call; reveal a second time directly to
    // cross the threshold (the second turn's c2 call dispatches the now-advertised
    // tool, so drive the count via the broker to keep the test deterministic).
    let _ = h.runtime.run_turn("history", &h.events, &h.cancel).await;
    h.broker.reveal("git_log");
    assert!(
        h.broker.graduated_names().contains(&"git_log".to_string()),
        "git_log should graduate after crossing the threshold"
    );
}

#[tokio::test]
async fn with_no_broker_every_tool_is_advertised() {
    // The rollback path: no broker ⇒ the full registry is advertised as before.
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(FakeProvider::new().text("done"));
    let mut runtime = SessionRuntime::new(
        provider.clone(),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Default, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(dir.path()),
        Workspace::new(dir.path()).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig::default(),
        Vec::new(),
    );
    let (events, _rx) = broadcast::channel(256);
    let _ = runtime
        .run_turn("hello", &events, &CancellationToken::new())
        .await;
    let names = advertised(&provider.requests());
    assert!(names.contains(&"git_commit".to_string()), "{names:?}");
    assert!(names.contains(&"fetch".to_string()), "{names:?}");
}
