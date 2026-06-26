//! Per-turn prefill measurements (subject: lean prefill).
//!
//! Two facts this pins, both measured offline against the `FakeProvider` that
//! records the exact request it was handed:
//! 1. The dominant per-turn prefill weight is the advertised tool schemas, and
//!    the pull-discovery broker (ADR-0031) cuts it substantially — the real
//!    convergence lever, since a leaner request leaves more budget per turn.
//! 2. Compaction trims-not-pads: a short transcript produces a small request,
//!    nowhere near the context ceiling — so prefill is only as large as the live
//!    conversation, contrary to the "always sits at the 24k ceiling" assumption.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use localpilot_harness::{SessionConfig, SessionRuntime};
use localpilot_llm::{FakeProvider, ModelRequest};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::{Broker, BrokerConfig, ToolLoad, ToolRegistry, ToolSearch};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Serialized byte size of the advertised tool schemas in the first request —
/// the prefill weight the broker acts on.
fn tool_schema_bytes(requests: &[ModelRequest]) -> usize {
    requests
        .first()
        .map(|request| {
            request
                .tools
                .iter()
                .map(|t| {
                    t.name.len()
                        + t.description.len()
                        + serde_json::to_string(&t.input_schema).map_or(0, |s| s.len())
                })
                .sum()
        })
        .unwrap_or(0)
}

fn tool_count(requests: &[ModelRequest]) -> usize {
    requests.first().map_or(0, |r| r.tools.len())
}

fn base_config() -> SessionConfig {
    SessionConfig {
        interactivity: Interactivity::NonInteractive,
        trusted: true,
        ..SessionConfig::default()
    }
}

/// A runtime with the full builtin tool set and **no** broker — every tool's
/// schema is advertised each turn (the rollback path). Returns the provider
/// handle so the recorded request can be inspected.
fn runtime_broker_off(
    root: &std::path::Path,
    provider: FakeProvider,
) -> (SessionRuntime, Arc<FakeProvider>) {
    let provider = Arc::new(provider);
    let runtime = SessionRuntime::new(
        provider.clone(),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Bypass, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(root),
        Workspace::new(root).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        base_config(),
        Vec::new(),
    );
    (runtime, provider)
}

/// A runtime with the broker installed, narrowing the advertised set to the core
/// working set (plus the broker's own tools).
fn runtime_broker_on(
    root: &std::path::Path,
    provider: FakeProvider,
) -> (SessionRuntime, Arc<FakeProvider>) {
    let provider = Arc::new(provider);
    let broker = Broker::new(BrokerConfig::default());
    let mut registry = ToolRegistry::with_builtins();
    registry.register(Box::new(ToolSearch::new(broker.clone())));
    registry.register(Box::new(ToolLoad::new(broker.clone())));
    broker.set_catalog(registry.catalog());
    let mut runtime = SessionRuntime::new(
        provider.clone(),
        registry,
        PermissionEngine::new(Profile::Bypass, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(root),
        Workspace::new(root).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        base_config(),
        Vec::new(),
    );
    runtime.set_broker(Some(broker));
    (runtime, provider)
}

#[test]
fn the_broker_substantially_cuts_advertised_tool_schema_prefill() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let cancel = CancellationToken::new();

    let (mut off, off_provider) = runtime_broker_off(root, FakeProvider::new().text("done"));
    let (events, _rx) = broadcast::channel(16);
    rt.block_on(off.run_turn("hello", &events, &cancel));
    let off_bytes = tool_schema_bytes(&off_provider.requests());
    let off_count = tool_count(&off_provider.requests());

    let (mut on, on_provider) = runtime_broker_on(root, FakeProvider::new().text("done"));
    let (events, _rx) = broadcast::channel(16);
    rt.block_on(on.run_turn("hello", &events, &cancel));
    let on_bytes = tool_schema_bytes(&on_provider.requests());
    let on_count = tool_count(&on_provider.requests());

    // Recorded measurement (visible with `--nocapture`).
    eprintln!(
        "prefill tool schemas — broker off: {off_count} tools / {off_bytes} bytes; \
         broker on: {on_count} tools / {on_bytes} bytes"
    );

    assert!(
        off_count > 0 && off_bytes > 0,
        "the off arm must advertise tools"
    );
    assert!(
        on_count < off_count,
        "broker on must advertise fewer tools ({on_count} vs {off_count})"
    );
    // Measured ~35% cut with the default core working set (21→12 tools,
    // ~14.2 KB→~9.2 KB); assert a meaningful floor of ≥20% so the lever is
    // pinned without coupling the test to an exact byte count.
    assert!(
        on_bytes * 5 < off_bytes * 4,
        "broker on must cut tool-schema prefill by ≥20% ({on_bytes} vs {off_bytes})"
    );
}

#[test]
fn a_short_transcript_does_not_pad_prefill_to_the_context_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let cancel = CancellationToken::new();

    // The default context budget is large (24k tokens ≈ ~96 KB of text), but a
    // one-line turn must produce a request only as big as its actual content —
    // compaction trims when over the limit, it never pads up to it.
    let (mut off, provider) = runtime_broker_off(root, FakeProvider::new().text("done"));
    let (events, _rx) = broadcast::channel(16);
    rt.block_on(off.run_turn("hi", &events, &cancel));

    let requests = provider.requests();
    let request = requests.first().expect("a recorded request");
    let message_bytes: usize = request
        .messages
        .iter()
        .map(|m| serde_json::to_string(m).map_or(0, |s| s.len()))
        .sum();
    eprintln!("short-turn message prefill: {message_bytes} bytes");
    assert!(
        message_bytes < 8_000,
        "a one-line turn must not be padded toward the 24k-token ceiling (got {message_bytes} bytes)"
    );
}
