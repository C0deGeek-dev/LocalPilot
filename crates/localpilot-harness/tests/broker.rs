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
    events: broadcast::Sender<RuntimeEvent>,
    cancel: CancellationToken,
}

fn build_broker_harness(provider: FakeProvider, config: BrokerConfig) -> BrokerHarness {
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
        SessionConfig::default(),
        Vec::new(),
    );
    runtime.set_broker(Some(broker.clone()));

    let (events, _rx) = broadcast::channel(256);
    BrokerHarness {
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
