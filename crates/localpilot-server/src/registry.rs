//! A server-owned registry of live [`SessionRuntime`]s.
//!
//! One server process hosts many sessions at once. Each session is an actor: it
//! is owned by exactly one task and driven through `&mut` (a turn takes
//! `&mut self`), so two turns on the same session can never overlap. The
//! registry hands out per-session handles that make that ownership explicit
//! without serialising *unrelated* sessions against each other.
//!
//! Two locks, deliberately layered:
//!
//! - A structural [`RwLock`] guards the id → handle map. It is held only long
//!   enough to read or mutate the map — never across a turn.
//! - A per-session async [`Mutex`] (inside each [`SessionHandle`]) is held for
//!   the whole of a turn, giving the single owner its exclusive `&mut`.
//!
//! Because those are different locks, a turn in flight on session A (holding
//! A's [`Mutex`]) does not block a `get`/`list`/`open_new` touching the map, and
//! does not block a turn on session B. The registry never constructs a
//! [`SessionRuntime`] itself: a caller supplies a [`SessionFactory`], which
//! keeps this crate off the heavy session-construction path.

use std::collections::HashMap;
use std::sync::Arc;

use localpilot_core::SessionId;
use localpilot_harness::SessionRuntime;
use localpilot_store::{Store, StoreError};
use tokio::sync::{Mutex, RwLock};

/// Each session is owned by one task, so its runtime must cross a task boundary.
/// A compile-time proof of `SessionRuntime: Send`; if the runtime ever gains a
/// non-`Send` field this stops compiling rather than failing at a spawn site.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<SessionRuntime>();
};

/// Errors from registry operations.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RegistryError {
    /// A session with this id is already registered. Returned instead of
    /// silently replacing a live runtime (which would strand its owner task).
    #[error("session {0} is already present in the registry")]
    AlreadyPresent(SessionId),
    /// No session with the requested id is registered.
    #[error("session not found in the registry")]
    NotFound,
    /// No session carries the requested name in the store index.
    #[error("no session is named {0:?}")]
    UnknownName(String),
    /// The underlying store failed (e.g. reading the event log on resume).
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The caller-supplied factory could not build a runtime.
    #[error("session factory failed: {0}")]
    Factory(String),
}

/// Builds a fresh [`SessionRuntime`] on demand. The registry owns *hosting* a
/// session, never *constructing* one — the heavy CLI/provider wiring lives
/// behind this trait so it stays out of this crate.
///
/// A freshly built runtime is already `SessionOpened::New` (its constructor
/// records that event), so [`SessionRegistry::open_new`] can insert it as-is,
/// while the resume paths build one and then replay a target id's event log
/// onto it.
pub trait SessionFactory: Send + Sync {
    /// Build a new runtime. The returned runtime has just been constructed and
    /// carries a fresh [`SessionId`].
    ///
    /// # Errors
    /// Returns [`RegistryError::Factory`] (or any error the implementation maps
    /// into it) if construction fails.
    fn create(&self) -> Result<SessionRuntime, RegistryError>;
}

/// Any suitable closure is a [`SessionFactory`], so callers can pass one inline
/// without declaring a type.
impl<F> SessionFactory for F
where
    F: Fn() -> Result<SessionRuntime, RegistryError> + Send + Sync,
{
    fn create(&self) -> Result<SessionRuntime, RegistryError> {
        self()
    }
}

/// A shared, independently lockable handle to one hosted session. The async
/// [`Mutex`] is held for the duration of a turn, so cloning the handle (cheap,
/// an [`Arc`] bump) lets several holders await the *same* session in turn while
/// never touching each other's sessions.
pub type SessionHandle = Arc<Mutex<SessionRuntime>>;

/// A process-local registry of live sessions, keyed by [`SessionId`].
///
/// Cloning is cheap (an [`Arc`] bump) and yields another handle onto the *same*
/// underlying map, so the registry can be shared across tasks.
#[derive(Clone, Default)]
pub struct SessionRegistry {
    inner: Arc<RwLock<HashMap<SessionId, SessionHandle>>>,
}

impl SessionRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a fresh session via `factory` and register it under its own
    /// [`SessionId`]. Returns the id it was registered under.
    ///
    /// # Errors
    /// Returns [`RegistryError::Factory`] if the factory fails, or
    /// [`RegistryError::AlreadyPresent`] if a session with that id is somehow
    /// already registered.
    pub async fn open_new(&self, factory: &dyn SessionFactory) -> Result<SessionId, RegistryError> {
        let runtime = factory.create()?;
        let id = runtime.session_id();
        self.insert(id, runtime).await
    }

    /// Build a fresh runtime via `factory`, then replay session `id`'s durable
    /// event log onto it (`load_session`), and register the result. The runtime
    /// adopts `id` as its own [`SessionId`] during the replay, so the map is
    /// keyed by the post-load id (read back, not assumed).
    ///
    /// # Errors
    /// Returns [`RegistryError::Factory`] if the factory fails,
    /// [`RegistryError::Store`] if the event log cannot be read, or
    /// [`RegistryError::AlreadyPresent`] if the id is already registered.
    pub async fn resume_by_id(
        &self,
        id: SessionId,
        factory: &dyn SessionFactory,
    ) -> Result<SessionId, RegistryError> {
        let mut runtime = factory.create()?;
        runtime.load_session(id)?;
        // `load_session` adopts the target id; key by what the runtime now
        // reports rather than trusting `id` blindly.
        let resumed = runtime.session_id();
        self.insert(resumed, runtime).await
    }

    /// Resolve `name` to a session id via the store index, then
    /// [`resume_by_id`](Self::resume_by_id).
    ///
    /// # Errors
    /// Returns [`RegistryError::UnknownName`] if no session carries `name`,
    /// [`RegistryError::Store`] if the index cannot be read, plus any error
    /// from [`resume_by_id`](Self::resume_by_id).
    pub async fn resume_by_name(
        &self,
        name: &str,
        store: &Store,
        factory: &dyn SessionFactory,
    ) -> Result<SessionId, RegistryError> {
        let entry = store
            .find_session_by_name(name)?
            .ok_or_else(|| RegistryError::UnknownName(name.to_string()))?;
        self.resume_by_id(entry.id, factory).await
    }

    /// Register a runtime that was built elsewhere, under its own id.
    ///
    /// [`open_new`](Self::open_new) covers the case where the registry should do
    /// the building. This covers the case where the caller already has a runtime
    /// and needs it hosted — a swarm worker, whose construction is decided by a
    /// spawn request the registry knows nothing about.
    ///
    /// # Errors
    /// [`RegistryError::AlreadyPresent`] if that id is already registered.
    pub async fn register(&self, runtime: SessionRuntime) -> Result<SessionId, RegistryError> {
        let id = runtime.session_id();
        self.insert(id, runtime).await
    }

    /// Look up a session's handle, cloning the [`Arc`]. The structural read-lock
    /// is held only for the clone, never across the returned handle's use, so
    /// this stays prompt even while another session's turn is in flight.
    pub async fn get(&self, id: SessionId) -> Option<SessionHandle> {
        self.inner.read().await.get(&id).cloned()
    }

    /// Remove a session from the map and return its handle, if present. The
    /// caller owns the returned handle and may `close()` the runtime through it.
    /// Other clones of the handle stay valid; the session is simply no longer
    /// discoverable via the registry.
    pub async fn remove(&self, id: SessionId) -> Option<SessionHandle> {
        self.inner.write().await.remove(&id)
    }

    /// The ids of all currently registered sessions, in unspecified order.
    pub async fn list(&self) -> Vec<SessionId> {
        self.inner.read().await.keys().copied().collect()
    }

    /// The number of registered sessions.
    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    /// Whether the registry holds no sessions.
    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.is_empty()
    }

    /// Insert `runtime` under `id`, refusing to clobber a live session. Holds
    /// the structural write-lock only for the check-and-insert.
    async fn insert(
        &self,
        id: SessionId,
        runtime: SessionRuntime,
    ) -> Result<SessionId, RegistryError> {
        let mut map = self.inner.write().await;
        if map.contains_key(&id) {
            return Err(RegistryError::AlreadyPresent(id));
        }
        map.insert(id, Arc::new(Mutex::new(runtime)));
        Ok(id)
    }
}
