//! A per-session host layer: multi-client event fanout plus lock-free
//! out-of-band cancel/steer for the in-flight turn.
//!
//! One [`SessionHost`] wraps one registry [`SessionHandle`] (an
//! `Arc<tokio::Mutex<SessionRuntime>>`). It exists to solve a problem the
//! registry alone cannot: a running turn holds the session's async
//! [`Mutex`](tokio::sync::Mutex) for its whole duration, so anything that had to
//! take that mutex to reach the turn — cancel, steer, a status read — would
//! block until the turn ended, which is exactly when it is least useful.
//!
//! The host sidesteps that by extracting the two control handles the runtime
//! already exposes **once, at construction**, so control never needs the
//! runtime mutex again:
//!
//! - **Fanout** rides a session-lifetime [`broadcast`] channel created in
//!   [`SessionHost::new`] (not per turn). [`subscribe`](SessionHost::subscribe)
//!   hands each client its own receiver; a driven turn streams every
//!   [`RuntimeEvent`] into that one sender, so every attached client — including
//!   one that attaches mid-turn — sees the same stream. Broadcast tracks its own
//!   receivers, so a dropped client prunes itself and never errors the driver.
//! - **Out-of-band control** reads a short [`std::sync::Mutex`] holding the
//!   current turn's [`CancellationToken`] (published by
//!   [`drive`](SessionHost::drive) *before* it awaits the turn) and the
//!   [`SteerQueue`] clone. [`cancel`](SessionHost::cancel) cancels the token;
//!   [`steer`](SessionHost::steer) pushes onto the queue. Neither ever touches
//!   the runtime mutex, so both land while `drive` holds it — the lock-free
//!   substrate for out-of-band steering and cancellation.
//!
//! The std mutex guarding the turn token is only ever held for a trivial
//! read/assign and is always dropped before any `.await`, so it never blocks a
//! turn and never trips `clippy::await_holding_lock`.

use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use localpilot_core::SessionId;
use localpilot_harness::{RuntimeEvent, SoftInterrupt, SteerQueue, StopReason};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::registry::SessionHandle;

/// Capacity of the session-lifetime event broadcast. Sized so an attached
/// client can fall well behind a chatty turn before it lags; a client that lags
/// anyway observes [`broadcast::error::RecvError::Lagged`] and resynchronises
/// rather than stalling the turn.
const EVENT_CAPACITY: usize = 1024;

/// A per-session host giving multiple clients a shared event stream and
/// lock-free out-of-band control over the in-flight turn.
///
/// Cloning is not offered: the host owns the session-lifetime broadcast sender
/// and the turn-token slot. Share it behind an [`Arc`](std::sync::Arc) when
/// several tasks (a driver, a control channel, several fanout consumers) need
/// it at once.
pub struct SessionHost {
    /// The hosted session. Locked only by [`drive`](Self::drive) for the whole
    /// of a turn; control paths never take this lock.
    runtime: SessionHandle,
    /// Session-lifetime fanout substrate. Created once, in [`new`](Self::new),
    /// so a client that subscribes mid-turn still receives subsequent events.
    events: broadcast::Sender<RuntimeEvent>,
    /// A clone of the runtime's steer queue, taken once at construction. Pushing
    /// onto it enqueues a soft interrupt the running turn drains at its next
    /// safe boundary — without touching the runtime mutex.
    steer: SteerQueue,
    /// The current turn's cancellation token, or `None` when idle. Guarded by a
    /// std mutex held only for a trivial read/assign, never across an `.await`.
    turn_cancel: Mutex<Option<CancellationToken>>,
    /// The instant of this session's most recent activity: its creation, then
    /// bumped at the start of every turn. Read by the server's session reaper to
    /// measure idle time. Guarded by a std mutex held only for a trivial
    /// read/assign, never across an `.await`.
    last_active: Mutex<Instant>,
    /// The hosted session's id, read once at construction.
    id: SessionId,
}

impl SessionHost {
    /// Build a host around a registry [`SessionHandle`].
    ///
    /// Locks the runtime exactly once, to clone its [`SteerQueue`] and read its
    /// [`SessionId`], then releases it. The event broadcast is created here and
    /// lives for as long as the host, so it is the shared substrate for every
    /// turn this host drives.
    pub async fn new(runtime: SessionHandle) -> Self {
        let (steer, id) = {
            let guard = runtime.lock().await;
            (guard.steer_queue(), guard.session_id())
        };
        // Drop the initial receiver immediately: subscribers attach on demand via
        // `subscribe`, and the sender alone keeps the channel open, so
        // `subscriber_count` starts at a truthful zero.
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Self {
            runtime,
            events,
            steer,
            turn_cancel: Mutex::new(None),
            last_active: Mutex::new(Instant::now()),
            id,
        }
    }

    /// The hosted session's id.
    #[must_use]
    pub fn id(&self) -> SessionId {
        self.id
    }

    /// Attach a client: a fresh receiver onto the session-lifetime event stream.
    ///
    /// A receiver attached mid-turn observes every event sent *after* it
    /// subscribes. A slow receiver that falls more than [`EVENT_CAPACITY`]
    /// events behind observes [`broadcast::error::RecvError::Lagged`] on its
    /// next `recv` and then resumes from the oldest retained event; it never
    /// blocks the turn.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.events.subscribe()
    }

    /// How many clients are currently attached. Reflects attach
    /// ([`subscribe`](Self::subscribe)) and detach (receiver drop) without any
    /// bookkeeping of our own — broadcast tracks its live receivers.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.events.receiver_count()
    }

    /// Drive one turn to completion, streaming its events to every attached
    /// client, and return why it stopped.
    ///
    /// Locks the runtime for the whole turn (the single-owner `&mut`), publishes
    /// a fresh [`CancellationToken`] into [`turn_cancel`](Self::turn_cancel)
    /// **before** awaiting the turn, and clears it on return. Because the token
    /// is published before the `.await`, [`cancel`](Self::cancel) and
    /// [`steer`](Self::steer) reach this turn the entire time it runs — even
    /// though this method holds the runtime mutex — since they read only the
    /// short token slot and the steer queue, never the runtime mutex.
    pub async fn drive(&self, input: &str) -> StopReason {
        self.touch();
        let token = CancellationToken::new();
        // Acquire the runtime mutex first, then publish this turn's token, so the
        // slot always names the turn that actually holds the mutex (a concurrent
        // `drive` waiting on the lock cannot clobber the running turn's token).
        let mut runtime = self.runtime.lock().await;
        self.set_turn_token(Some(token.clone()));
        let reason = runtime.run_turn(input, &self.events, &token).await;
        self.set_turn_token(None);
        reason
    }

    /// Cancel the in-flight turn, if any, without taking the runtime mutex.
    ///
    /// Returns `true` if a turn was in flight (a token was published), `false`
    /// when idle. Cancelling is a one-way signal the running turn observes at
    /// its next cancellation check — a safe boundary or an executing tool, both
    /// of which race the token — so this returns promptly even while
    /// [`drive`](Self::drive) holds the runtime mutex.
    pub fn cancel(&self) -> bool {
        let guard = self
            .turn_cancel
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match guard.as_ref() {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    /// Steer the in-flight (or next) turn without taking the runtime mutex.
    ///
    /// The text is enqueued as a user soft interrupt on the runtime's own steer
    /// queue; the running turn admits it at its next safe provider-turn boundary
    /// (after a tool batch, or before the next provider call), injecting it as a
    /// user-role message. Enqueuing touches only the queue's own lock, so it
    /// lands while [`drive`](Self::drive) holds the runtime mutex.
    pub fn steer(&self, text: impl Into<String>) {
        self.steer.push(text);
    }

    /// Inject a non-user message into the in-flight (or next) turn.
    ///
    /// Same substrate and same safe-point discipline as [`steer`](Self::steer),
    /// but the interrupt carries its own source, so a peer's message or a
    /// background report is labelled as such in the transcript instead of
    /// arriving indistinguishable from something the user typed. That
    /// distinction is not cosmetic: a model that cannot tell the two apart will
    /// treat another agent's suggestion as an instruction.
    pub fn inject(&self, interrupt: SoftInterrupt) {
        self.steer.push_interrupt(interrupt);
    }

    /// Whether a turn is in flight, read without blocking on it.
    ///
    /// This reads the short turn-token slot, so it never contends the runtime
    /// mutex a running turn holds — a strictly non-blocking status read (a
    /// `try_lock` on the runtime would report the same "held" state, but this
    /// never so much as reaches for that lock).
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.turn_cancel
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    /// The instant of this session's most recent activity — its creation, or the
    /// start of its last turn. The server's session reaper measures idle time
    /// from here; a session idle past the configured timeout is a reap candidate.
    #[must_use]
    pub fn last_active(&self) -> Instant {
        *self
            .last_active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Record activity now (a turn is starting). Held only for the assignment,
    /// never across an `.await`, so it never contends the runtime mutex.
    fn touch(&self) {
        *self
            .last_active
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    /// A non-blocking snapshot of the host: its id, whether a turn is in flight,
    /// and how many clients are attached.
    #[must_use]
    pub fn status(&self) -> HostStatus {
        HostStatus {
            id: self.id,
            busy: self.is_busy(),
            subscribers: self.subscriber_count(),
        }
    }

    /// Route a [`Control`] request to the matching host method.
    ///
    /// This is the seam a transport dispatches decoded control frames through:
    /// every arm maps to a lock-free method, so a control request is answered
    /// while a turn holds the runtime mutex.
    pub fn control(&self, request: Control) -> ControlOutcome {
        match request {
            Control::Cancel => ControlOutcome::Cancelled(self.cancel()),
            Control::Steer(text) => {
                self.steer(text);
                ControlOutcome::Steered
            }
            Control::Status => ControlOutcome::Status(self.status()),
        }
    }

    /// Publish or clear the current turn's cancellation token. The guard is held
    /// only for the assignment and dropped before any caller `.await`.
    fn set_turn_token(&self, token: Option<CancellationToken>) {
        let mut guard = self
            .turn_cancel
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *guard = token;
    }
}

/// An out-of-band control request against a hosted session, decoupled from any
/// particular transport encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Control {
    /// Cancel the in-flight turn.
    Cancel,
    /// Steer the in-flight (or next) turn with user text.
    Steer(String),
    /// Read a non-blocking status snapshot.
    Status,
}

/// The result of routing a [`Control`] request through
/// [`SessionHost::control`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ControlOutcome {
    /// A cancel was routed; the flag is whether a turn was in flight.
    Cancelled(bool),
    /// A steer was enqueued.
    Steered,
    /// A status snapshot.
    Status(HostStatus),
}

/// A non-blocking snapshot of a [`SessionHost`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostStatus {
    /// The hosted session's id.
    pub id: SessionId,
    /// Whether a turn is in flight.
    pub busy: bool,
    /// How many clients are attached to the event stream.
    pub subscribers: usize,
}
