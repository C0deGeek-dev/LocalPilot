//! Session-registry behaviour: create/lookup/remove by id, resume-by-name
//! round-trip from the event log, and per-session state isolation. The registry
//! never builds a runtime itself, so every test supplies a factory that mirrors
//! how the harness's own tests construct a `SessionRuntime` (a `FakeProvider`,
//! builtin tools, a bypass permission engine + sandbox workspace, a recovery
//! engine, over a `tempfile` temp-dir store).
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use localpilot_harness::{RuntimeEvent, SessionConfig, SessionRuntime};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_server::registry::{RegistryError, SessionFactory, SessionHandle, SessionRegistry};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use tempfile::TempDir;
use tokio::sync::broadcast;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

/// A short bound for "must return promptly" assertions: a call blocked on a
/// session's own lock would sit here until the turn finished, so exceeding it
/// fails the test rather than hanging it.
const PROMPT: Duration = Duration::from_secs(5);

/// Builds `SessionRuntime`s over a single shared temp-dir store and provider, so
/// several sessions share one workspace on disk (resume reads it back) while
/// keeping independent in-memory runtimes. Holds the `TempDir` alive for the
/// whole test.
struct FakeFactory {
    dir: Arc<TempDir>,
    provider: Arc<FakeProvider>,
}

impl FakeFactory {
    fn new(provider: FakeProvider) -> Self {
        Self {
            dir: Arc::new(tempfile::tempdir().unwrap()),
            provider: Arc::new(provider),
        }
    }

    fn store(&self) -> Store {
        Store::open(self.dir.path())
    }

    fn build(&self) -> Result<SessionRuntime, RegistryError> {
        let root = self.dir.path();
        let workspace =
            Workspace::new(root).map_err(|err| RegistryError::Factory(err.to_string()))?;
        Ok(SessionRuntime::new(
            self.provider.clone(),
            ToolRegistry::with_builtins(),
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
}

impl SessionFactory for FakeFactory {
    fn create(&self) -> Result<SessionRuntime, RegistryError> {
        self.build()
    }
}

/// Drive one turn through a handle to completion, exactly as an owner task
/// would: take the per-session lock and hold it across `run_turn().await`.
async fn drive_turn(handle: &SessionHandle, input: &str) {
    let (events, _rx) = broadcast::channel::<RuntimeEvent>(1024);
    let cancel = CancellationToken::new();
    let mut runtime = handle.lock().await;
    runtime.run_turn(input, &events, &cancel).await;
}

// --- 02.1 create / lookup / remove; a turn on A never blocks get(B)/list() ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_lookup_remove_and_turn_does_not_block_other_sessions() {
    let factory = FakeFactory::new(FakeProvider::new().text("a done"));
    let registry = SessionRegistry::new();

    // Two sessions created via the factory and looked up by id.
    let a = registry.open_new(&factory).await.unwrap();
    let b = registry.open_new(&factory).await.unwrap();
    assert_ne!(a, b, "each created session gets its own id");
    assert_eq!(registry.len().await, 2);
    assert!(!registry.is_empty().await);
    let ids = registry.list().await;
    assert!(ids.contains(&a) && ids.contains(&b));
    assert!(registry.get(a).await.is_some());
    assert!(registry.get(b).await.is_some());

    let handle_a = registry.get(a).await.unwrap();

    // Structural proof: holding A's per-session Mutex (what a turn does) must not
    // block the map-level operations, because they take the RwLock, not A's
    // Mutex. If the two were the same lock, these would deadlock past PROMPT.
    {
        let _a_guard = handle_a.lock().await;
        assert!(
            timeout(PROMPT, registry.get(b)).await.unwrap().is_some(),
            "get(B) blocked while A's turn-lock was held"
        );
        assert_eq!(
            timeout(PROMPT, registry.list()).await.unwrap().len(),
            2,
            "list() blocked while A's turn-lock was held"
        );
        // get(A) only clones the Arc; it never locks the inner Mutex, so it is
        // prompt even while that Mutex is held.
        assert!(timeout(PROMPT, registry.get(a)).await.unwrap().is_some());
    }

    // Now spawn a real FakeProvider turn on A and, while it is in flight, assert
    // get(B) still returns promptly.
    let turn = {
        let handle_a = handle_a.clone();
        tokio::spawn(async move { drive_turn(&handle_a, "hello").await })
    };
    assert!(
        timeout(PROMPT, registry.get(b)).await.unwrap().is_some(),
        "get(B) blocked while A's turn was running"
    );
    turn.await.unwrap();

    // Remove B; it is gone from the map, A remains.
    let removed = registry.remove(b).await;
    assert!(removed.is_some());
    assert!(registry.get(b).await.is_none());
    assert!(
        registry.remove(b).await.is_none(),
        "double remove is a no-op"
    );
    assert_eq!(registry.list().await, vec![a]);
    assert_eq!(registry.len().await, 1);
}

// --- 02.2 open + resume-by-name round-trip from the event log ----------------

#[tokio::test]
async fn resume_by_name_round_trips_from_the_event_log() {
    let factory = FakeFactory::new(FakeProvider::new().text("remembered reply"));
    let store = factory.store();
    let registry = SessionRegistry::new();

    // Open a session and drive a turn so the event log has content to replay.
    let id = registry.open_new(&factory).await.unwrap();
    let handle = registry.get(id).await.unwrap();
    drive_turn(&handle, "say something").await;
    drop(handle);

    // Give it a name (this also registers it in the store index).
    store.set_session_name(id, "greeting").unwrap();

    // Drop it from the registry entirely.
    assert!(registry.remove(id).await.is_some());
    assert!(registry.get(id).await.is_none());
    assert!(registry.is_empty().await);

    // Resume by name: rebuilt from the log, same id, back in the registry.
    let resumed = registry
        .resume_by_name("greeting", &store, &factory)
        .await
        .unwrap();
    assert_eq!(resumed, id, "resume preserves the original session id");
    assert!(registry.get(id).await.is_some());
    assert_eq!(registry.len().await, 1);

    // The replayed transcript carries the assistant reply from before the drop.
    let handle = registry.get(id).await.unwrap();
    let replayed = handle.lock().await.last_assistant_text();
    assert_eq!(
        replayed.as_deref(),
        Some("remembered reply"),
        "resumed session did not replay its event log"
    );

    // An unknown name is a typed error, not a panic.
    let err = registry
        .resume_by_name("no-such-session", &store, &factory)
        .await
        .unwrap_err();
    assert!(matches!(err, RegistryError::UnknownName(name) if name == "no-such-session"));
}

// --- 02.3 per-session isolation ----------------------------------------------

#[tokio::test]
async fn sessions_have_independent_runtimes_and_state() {
    // A closure factory (exercises the blanket `SessionFactory` impl) sharing one
    // provider + workspace across the sessions it builds, so any state bleed
    // would have to come through the per-session runtime, not the factory.
    let dir = Arc::new(tempfile::tempdir().unwrap());
    let provider = Arc::new(FakeProvider::new().text("alpha only"));
    let factory = {
        let dir = dir.clone();
        let provider = provider.clone();
        move || -> Result<SessionRuntime, RegistryError> {
            let root = dir.path();
            let workspace =
                Workspace::new(root).map_err(|err| RegistryError::Factory(err.to_string()))?;
            Ok(SessionRuntime::new(
                provider.clone(),
                ToolRegistry::with_builtins(),
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
    let a = registry.open_new(&factory).await.unwrap();
    let b = registry.open_new(&factory).await.unwrap();
    assert_ne!(a, b, "distinct sessions have distinct ids");

    let handle_a = registry.get(a).await.unwrap();
    let handle_b = registry.get(b).await.unwrap();

    // Mutate only A: run a turn that appends an assistant reply to A's transcript.
    drive_turn(&handle_a, "hi").await;

    // A's state changed; B's did not — the mutable per-session state is not shared
    // even though the provider and workspace are.
    {
        let runtime_a = handle_a.lock().await;
        assert_eq!(runtime_a.session_id(), a);
        assert_eq!(
            runtime_a.last_assistant_text().as_deref(),
            Some("alpha only")
        );
    }
    {
        let runtime_b = handle_b.lock().await;
        assert_eq!(runtime_b.session_id(), b);
        assert_eq!(
            runtime_b.last_assistant_text(),
            None,
            "a turn on A must not appear in B's transcript"
        );
    }
}
