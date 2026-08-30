//! Connection-scoped attach dispatch: `open_new` binds a fresh session,
//! `resume_name`/`resume_id` rebind an existing one to the same id, and an
//! unknown id or name resolves to a typed error instead of a panic (or a silent
//! ghost session). The factory mirrors the registry tests: a `FakeProvider`,
//! builtin tools, a bypass permission engine + sandbox workspace, a recovery
//! engine, over a single `tempfile` temp-dir store shared across the sessions it
//! builds so resume reads back what an earlier turn persisted.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use localpilot_core::SessionId;
use localpilot_harness::{RuntimeEvent, SessionConfig, SessionRuntime};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_rpc::AttachTarget;
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_server::attach::{attach, AttachError};
use localpilot_server::registry::{RegistryError, SessionFactory, SessionHandle, SessionRegistry};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use tempfile::TempDir;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Builds `SessionRuntime`s over one shared temp-dir store and provider, so
/// several sessions share a workspace on disk (resume reads it back) while
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

/// Drive one turn through a handle to completion, so the session has a durable
/// event log (and store-index entry) to resume from.
async fn drive_turn(handle: &SessionHandle, input: &str) {
    let (events, _rx) = broadcast::channel::<RuntimeEvent>(1024);
    let cancel = CancellationToken::new();
    let mut runtime = handle.lock().await;
    runtime.run_turn(input, &events, &cancel).await;
}

// --- open-new binds a fresh session -----------------------------------------

#[tokio::test]
async fn attach_open_new_creates_and_binds_a_session() {
    let factory = FakeFactory::new(FakeProvider::new().text("hi"));
    let store = factory.store();
    let registry = SessionRegistry::new();

    let id = attach(AttachTarget::OpenNew, &registry, &factory, &store)
        .await
        .unwrap();

    // The returned id is bound: the caller can build a host from it.
    assert!(registry.get(id).await.is_some());
    assert_eq!(registry.len().await, 1);

    // Two open-news are two distinct bound sessions.
    let other = attach(AttachTarget::OpenNew, &registry, &factory, &store)
        .await
        .unwrap();
    assert_ne!(id, other);
    assert_eq!(registry.len().await, 2);
}

// --- resume-by-name rebinds the same id -------------------------------------

#[tokio::test]
async fn attach_resume_name_rebinds_the_same_id() {
    let factory = FakeFactory::new(FakeProvider::new().text("remembered reply"));
    let store = factory.store();
    let registry = SessionRegistry::new();

    // Open + drive a turn so there is a log to replay, name it, then drop it
    // from the registry entirely.
    let id = attach(AttachTarget::OpenNew, &registry, &factory, &store)
        .await
        .unwrap();
    let handle = registry.get(id).await.unwrap();
    drive_turn(&handle, "say something").await;
    drop(handle);
    store.set_session_name(id, "greeting").unwrap();
    assert!(registry.remove(id).await.is_some());
    assert!(registry.is_empty().await);

    // Attach by name: same id, back in the registry, transcript replayed.
    let resumed = attach(
        AttachTarget::ResumeName {
            name: "greeting".to_string(),
        },
        &registry,
        &factory,
        &store,
    )
    .await
    .unwrap();
    assert_eq!(resumed, id, "resume-by-name preserves the session id");
    let handle = registry.get(id).await.unwrap();
    assert_eq!(
        handle.lock().await.last_assistant_text().as_deref(),
        Some("remembered reply"),
    );
}

// --- resume-by-id rebinds the same id ---------------------------------------

#[tokio::test]
async fn attach_resume_id_rebinds_the_same_id() {
    let factory = FakeFactory::new(FakeProvider::new().text("logged reply"));
    let store = factory.store();
    let registry = SessionRegistry::new();

    // A driven turn persists the session into the store index, so the known-id
    // guard passes on resume.
    let id = attach(AttachTarget::OpenNew, &registry, &factory, &store)
        .await
        .unwrap();
    let handle = registry.get(id).await.unwrap();
    drive_turn(&handle, "remember this").await;
    drop(handle);
    assert!(registry.remove(id).await.is_some());
    assert!(registry.is_empty().await);

    let resumed = attach(
        AttachTarget::ResumeId { session_id: id },
        &registry,
        &factory,
        &store,
    )
    .await
    .unwrap();
    assert_eq!(resumed, id, "resume-by-id preserves the session id");
    assert!(registry.get(id).await.is_some());
}

// --- unknown id / name are typed errors, never a panic ----------------------

#[tokio::test]
async fn attach_resume_id_with_unknown_id_is_a_typed_error() {
    let factory = FakeFactory::new(FakeProvider::new().text("unused"));
    let store = factory.store();
    let registry = SessionRegistry::new();

    let ghost = SessionId::new();
    let err = attach(
        AttachTarget::ResumeId { session_id: ghost },
        &registry,
        &factory,
        &store,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, AttachError::UnknownId(id) if id == ghost),
        "unexpected error: {err:?}",
    );
    // The guard held: no empty ghost session was minted under the unknown id.
    assert!(registry.is_empty().await);
    assert!(registry.get(ghost).await.is_none());
}

#[tokio::test]
async fn attach_resume_name_with_unknown_name_is_a_typed_error() {
    let factory = FakeFactory::new(FakeProvider::new().text("unused"));
    let store = factory.store();
    let registry = SessionRegistry::new();

    let err = attach(
        AttachTarget::ResumeName {
            name: "no-such-session".to_string(),
        },
        &registry,
        &factory,
        &store,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, AttachError::UnknownName(name) if name == "no-such-session"),
        "unexpected error: {err:?}",
    );
    assert!(registry.is_empty().await);
}
