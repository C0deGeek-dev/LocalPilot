//! A turn records the memories its context hooks used, for the local inspector.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use localpilot_harness::{ContextHook, SessionConfig, SessionRuntime};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::{MemoryUsed, SessionEventKind, Store};
use localpilot_tools::ToolRegistry;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// A context hook that reports a memory as used without changing what it injects.
struct StubMemoryHook;

impl ContextHook for StubMemoryHook {
    fn name(&self) -> &str {
        "stub-memory"
    }

    fn context_for(&self, _prompt: &str) -> Option<String> {
        Some("seeded context".to_string())
    }

    fn memories_used(&self, _prompt: &str) -> Vec<MemoryUsed> {
        vec![MemoryUsed {
            id: "mem-7".to_string(),
            score: 42,
            layer: "memory".to_string(),
        }]
    }
}

#[test]
fn run_turn_records_memories_used_from_context_hooks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    let provider = FakeProvider::new().text("done");
    let mut runtime = SessionRuntime::new(
        Arc::new(provider),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Bypass, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(root),
        Workspace::new(root).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            interactivity: Interactivity::NonInteractive,
            trusted: true,
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    runtime
        .hooks_mut()
        .register_context_hook(Arc::new(StubMemoryHook));
    let session = runtime.session_id();

    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(runtime.run_turn("anything", &events, &cancel));

    let logged = Store::open(root).read_events(session).unwrap();
    let used: Vec<MemoryUsed> = logged
        .iter()
        .filter_map(|event| match &event.kind {
            SessionEventKind::MemoriesUsed { memories } => Some(memories.clone()),
            _ => None,
        })
        .flatten()
        .collect();

    assert_eq!(used.len(), 1, "the turn must record the hook's used memory");
    assert_eq!(used[0].id, "mem-7");
    assert_eq!(used[0].layer, "memory");
}

#[test]
fn a_turn_with_no_used_memories_records_no_event() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // No context hook registered → no memories used → no MemoriesUsed event.
    let provider = FakeProvider::new().text("done");
    let mut runtime = SessionRuntime::new(
        Arc::new(provider),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Bypass, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(root),
        Workspace::new(root).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            interactivity: Interactivity::NonInteractive,
            trusted: true,
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    let session = runtime.session_id();

    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(runtime.run_turn("anything", &events, &cancel));

    let logged = Store::open(root).read_events(session).unwrap();
    assert!(
        !logged
            .iter()
            .any(|event| matches!(event.kind, SessionEventKind::MemoriesUsed { .. })),
        "an empty used-set must record nothing"
    );
}
