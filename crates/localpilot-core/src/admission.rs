//! Whether a session runtime may execute and mutate its session.
//!
//! One [`AdmissionGate`] is shared by a runtime and every handle it gives out
//! (reasoning effort, permission engine, steering queue). A mutation runs only
//! through [`AdmissionGate::mutate`], which holds the gate for the whole
//! mutation, so a state change (freezing the session for a hand-over) can never
//! interleave with one.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// The admission state of one runtime's session.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Admission {
    /// The runtime owns the session.
    Owned,
    /// A hand-over froze the session; nothing may execute or change it.
    Preparing { transfer: String },
    /// The session was handed over (terminal for that session).
    Released { transfer: String },
    /// The runtime does not own the session: it could not claim it, or a write
    /// found it owned elsewhere (terminal for that session).
    Lost,
    /// The runtime closed its session and let it go; nothing runs until it
    /// claims a session again (a new one, or by resuming one).
    Closed,
}

/// A shared, lockable admission state.
#[derive(Debug, Clone)]
pub struct AdmissionGate(Arc<Mutex<Admission>>);

impl Default for AdmissionGate {
    fn default() -> Self {
        Self::new(Admission::Owned)
    }
}

impl AdmissionGate {
    /// A gate starting in `state`.
    #[must_use]
    pub fn new(state: Admission) -> Self {
        Self(Arc::new(Mutex::new(state)))
    }

    /// Hold the gate: no mutation runs until the guard is dropped, and the
    /// holder may change the state.
    pub fn lock(&self) -> MutexGuard<'_, Admission> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The current state.
    #[must_use]
    pub fn state(&self) -> Admission {
        self.lock().clone()
    }

    /// Whether the runtime owns its session.
    #[must_use]
    pub fn is_owned(&self) -> bool {
        matches!(*self.lock(), Admission::Owned)
    }

    /// Replace the state.
    pub fn set(&self, state: Admission) {
        *self.lock() = state;
    }

    /// Run `mutate` only while the session is owned, holding the gate for the
    /// whole mutation. `None` means it was refused and did not run.
    pub fn mutate<T>(&self, mutate: impl FnOnce() -> T) -> Option<T> {
        let state = self.lock();
        matches!(*state, Admission::Owned).then(mutate)
    }

    /// Hold the gate while the session is owned (`None` otherwise). A caller
    /// mutates while holding the returned guard and must not call anything
    /// that takes the gate again (it is not reentrant).
    pub fn admit(&self) -> Option<MutexGuard<'_, Admission>> {
        let state = self.lock();
        matches!(*state, Admission::Owned).then_some(state)
    }

    /// Whether two handles share one gate.
    #[must_use]
    pub fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
