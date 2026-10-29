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
//!
//! **Admission** ([`submit`](SessionHost::submit)) decides, under one short std
//! mutex, whether input starts a turn, joins the bounded follow-up queue, or is
//! refused because a turn is running. The loop that drives admitted turns pops
//! the next follow-up under that same lock, so starting, queueing and draining
//! never race each other, and [`cancel`](SessionHost::cancel) can reach a
//! follow-up that was popped but whose turn has not published its token yet.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use localpilot_core::{SessionId, TokenUsage};
use localpilot_harness::{RuntimeEvent, SessionRuntime, SoftInterrupt, SteerQueue, StopReason};
use localpilot_store::SessionEventKind;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::registry::SessionHandle;

/// Capacity of the session-lifetime event broadcast. Sized so an attached
/// client can fall well behind a chatty turn before it lags; a client that lags
/// anyway observes [`broadcast::error::RecvError::Lagged`] and resynchronises
/// rather than stalling the turn.
const EVENT_CAPACITY: usize = 1024;

/// How many follow-ups may wait behind a running turn.
pub const FOLLOW_UP_CAPACITY: usize = 16;

/// Admission state: whether the admission loop is driving turns, the
/// follow-ups waiting for it, and the token of a follow-up it has popped but
/// not yet started.
#[derive(Default)]
struct Admission {
    running: bool,
    follow_ups: VecDeque<String>,
    starting: Option<CancellationToken>,
    /// Set once by the reaper: the session is being closed and admits nothing.
    closed: bool,
    /// A session command (compact, clear, …) holds the session exclusively.
    command: bool,
}

/// What [`SessionHost::submit`] did with the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    /// The session was idle; a turn is starting now.
    Started,
    /// A turn is running; the input waits as a follow-up.
    Queued,
    /// A turn is running and the input asked to run now.
    Busy,
    /// A turn is running and the follow-up queue is full.
    QueueFull,
    /// The session has been closed and admits no more input.
    Closed,
}

/// What [`SessionHost::cancel_all`] stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled {
    /// A turn was running, or about to start, and was cancelled.
    pub turn: bool,
    /// Follow-ups that were waiting and will not run.
    pub dropped_follow_ups: usize,
}

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
    /// Admission state for [`submit`](Self::submit). Guarded by a std mutex held
    /// only for short reads/assignments, never across an `.await`.
    admission: Mutex<Admission>,
    /// Client interventions (steer, cancel, permission answers) waiting to be
    /// written to the session log, which needs the runtime mutex.
    interventions: Mutex<Vec<SessionEventKind>>,
    /// The hosted session's id, read once at construction.
    id: SessionId,
}

/// The exact per-turn output captured by
/// [`drive_captured`](SessionHost::drive_captured): why the turn stopped, the
/// assistant text it produced (`None` when the turn produced none — never a
/// stale prior turn's), and the tokens it spent. What that text *means* is the
/// caller's concern; this host layer stays free of any protocol reading of it.
pub(crate) struct DriveCapture {
    pub(crate) reason: StopReason,
    pub(crate) assistant_text: Option<String>,
    pub(crate) usage: TokenUsage,
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
            admission: Mutex::new(Admission::default()),
            interventions: Mutex::new(Vec::new()),
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
        self.drive_captured(input).await.reason
    }

    /// Drive one turn and capture its exact assistant text and token spend, read
    /// from the runtime **before** the turn's lock is released.
    ///
    /// Same lock/token lifecycle as [`drive`](Self::drive): the fresh
    /// [`CancellationToken`] is published into
    /// [`turn_cancel`](Self::turn_cancel) *before* the `.await` and cleared on
    /// return, so [`cancel`](Self::cancel) and [`steer`](Self::steer) reach the
    /// turn the whole time it runs. The capture is read under the same runtime
    /// guard, so it is *this* turn's output — never a later turn's, and (because
    /// the runtime scopes the capture to the turn) never a stale prior message
    /// when this turn produced no assistant text.
    pub(crate) async fn drive_captured(&self, input: &str) -> DriveCapture {
        self.drive_with(input, CancellationToken::new()).await
    }

    /// [`drive_captured`](Self::drive_captured) with a caller-made token, so an
    /// admitted turn can be cancelled before it publishes the token itself.
    async fn drive_with(&self, input: &str, token: CancellationToken) -> DriveCapture {
        self.touch();
        // Acquire the runtime mutex first, then publish this turn's token, so the
        // slot always names the turn that actually holds the mutex (a concurrent
        // `drive` waiting on the lock cannot clobber the running turn's token).
        let mut runtime = self.runtime.lock().await;
        self.set_turn_token(Some(token.clone()));
        self.admission().starting = None;
        self.flush_interventions(&mut runtime);
        // Mark the turn's start in the same ordered stream as its events, so a
        // consumer attributes every later event to this turn.
        let _ = self.events.send(RuntimeEvent::TurnStarted);
        let reason = runtime.run_turn(input, &self.events, &token).await;
        self.flush_interventions(&mut runtime);
        let assistant_text = runtime.current_turn_assistant_text();
        let usage = runtime.current_turn_usage();
        self.set_turn_token(None);
        DriveCapture {
            reason,
            assistant_text,
            usage,
        }
    }

    /// Admit user input without taking the runtime mutex.
    ///
    /// When the session is idle the input starts a turn on its own task, and
    /// every follow-up queued behind it runs after it in order. When a turn is
    /// running, a follow-up (`queue_if_busy`) waits in a bounded queue and
    /// anything else is refused as [`Admit::Busy`]. Events from every admitted
    /// turn reach all attached clients through the host broadcast.
    pub fn submit(self: &Arc<Self>, text: String, queue_if_busy: bool) -> Admit {
        let token = {
            let mut admission = self.admission();
            if admission.closed {
                return Admit::Closed;
            }
            if admission.command {
                // A session command owns the session; nothing is admitted
                // until it finishes.
                return Admit::Busy;
            }
            // Lock order: admission, then the turn-token slot. Nothing takes
            // them the other way round while holding both.
            if admission.running || self.turn_in_flight() {
                if !queue_if_busy {
                    return Admit::Busy;
                }
                if admission.follow_ups.len() >= FOLLOW_UP_CAPACITY {
                    return Admit::QueueFull;
                }
                admission.follow_ups.push_back(text);
                if !admission.running {
                    // A turn driven outside admission is running: start the
                    // loop so the follow-up runs once that turn releases the
                    // session.
                    admission.running = true;
                    let token = CancellationToken::new();
                    admission.starting = Some(token.clone());
                    let next = admission.follow_ups.pop_front();
                    drop(admission);
                    if let Some(next) = next {
                        self.spawn_admitted(next, token);
                    }
                }
                return Admit::Queued;
            }
            admission.running = true;
            let token = CancellationToken::new();
            admission.starting = Some(token.clone());
            token
        };
        self.spawn_admitted(text, token);
        Admit::Started
    }

    /// Drive `text`, then each queued follow-up, until the queue is empty.
    fn spawn_admitted(self: &Arc<Self>, text: String, token: CancellationToken) {
        let host = Arc::clone(self);
        tokio::spawn(async move {
            let mut next = Some((text, token));
            while let Some((text, token)) = next.take() {
                // A follow-up cancelled between being popped and starting never
                // runs; `cancel_all` counted it as dropped.
                if !token.is_cancelled() {
                    host.drive_with(&text, token).await;
                }
                let mut admission = host.admission();
                admission.starting = None;
                match admission.follow_ups.pop_front() {
                    Some(text) => {
                        let token = CancellationToken::new();
                        admission.starting = Some(token.clone());
                        next = Some((text, token));
                    }
                    None => admission.running = false,
                }
            }
        });
    }

    /// Cancel the in-flight turn, if any, without taking the runtime mutex.
    ///
    /// Returns `true` if a turn was in flight or about to start, `false` when
    /// idle. Waiting follow-ups are dropped too (see
    /// [`cancel_all`](Self::cancel_all)). Cancelling is a one-way signal the
    /// running turn observes at its next cancellation check — a safe boundary
    /// or an executing tool, both of which race the token — so this returns
    /// promptly even while [`drive`](Self::drive) holds the runtime mutex.
    pub fn cancel(&self) -> bool {
        self.cancel_all().turn
    }

    /// Cancel the in-flight turn and drop every waiting follow-up. A Stop means
    /// stop: queued work does not start the moment the cancelled turn ends.
    pub fn cancel_all(&self) -> Cancelled {
        let (starting, dropped_follow_ups) = {
            let mut admission = self.admission();
            let dropped = admission.follow_ups.len();
            admission.follow_ups.clear();
            (admission.starting.take(), dropped)
        };
        let mut turn = false;
        if let Some(token) = starting {
            token.cancel();
            turn = true;
        }
        let guard = self
            .turn_cancel
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(token) = guard.as_ref() {
            token.cancel();
            turn = true;
        }
        Cancelled {
            turn,
            dropped_follow_ups,
        }
    }

    /// How many follow-ups are waiting behind the running turn.
    #[must_use]
    pub fn queued_follow_ups(&self) -> usize {
        self.admission().follow_ups.len()
    }

    /// Record a client intervention in the session log, attributed to
    /// `client`. Written at once when no turn holds the runtime; otherwise
    /// kept until the running turn ends (or the session is reaped).
    pub fn record_intervention(&self, action: &str, detail: String, client: &str) {
        let activity = self.is_busy().then(|| "turn running".to_string());
        self.interventions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(SessionEventKind::DriverIntervention {
                action: action.to_string(),
                detail,
                activity,
                client: client.to_string(),
            });
        if let Ok(mut runtime) = self.runtime.try_lock() {
            self.flush_interventions(&mut runtime);
        }
    }

    /// Write every waiting intervention to the session log. The caller holds
    /// the runtime.
    pub fn flush_interventions(&self, runtime: &mut SessionRuntime) {
        let waiting = std::mem::take(
            &mut *self
                .interventions
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        for kind in waiting {
            runtime.record_event(kind);
        }
    }

    fn admission(&self) -> std::sync::MutexGuard<'_, Admission> {
        self.admission
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
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

    /// Whether the session has work: a turn in flight, or admitted input that
    /// is about to start or waiting behind it. Read without blocking on a turn.
    ///
    /// This reads the short admission and turn-token slots (one at a time), so
    /// it never contends the runtime mutex a running turn holds — a strictly
    /// non-blocking status read.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        let busy = {
            let admission = self.admission();
            admission.running || admission.command
        };
        busy || self.turn_in_flight()
    }

    /// Reserve the session for one command that needs the runtime while no
    /// turn runs (compacting or clearing the context, say). Decided under the
    /// admission lock that [`submit`](Self::submit) and
    /// [`close_admission`](Self::close_admission) use, so a reserved command
    /// never overlaps admitted work, the reaper never closes the session under
    /// it, and input arriving meanwhile is refused as busy. `None` when the
    /// session has work, another command runs, or it is closed. Dropping the
    /// reservation (also when its future is cancelled) releases the session.
    pub fn reserve_command(&self) -> Option<CommandReservation<'_>> {
        let mut admission = self.admission();
        if admission.closed || admission.running || admission.command || self.turn_in_flight() {
            return None;
        }
        admission.command = true;
        Some(CommandReservation { host: self })
    }

    /// Deliver an event to every subscriber, as if the session had emitted it.
    /// For hosts reporting the effect of a session command (new context usage
    /// after a compaction) in the same ordered stream as turn events.
    pub fn publish(&self, event: RuntimeEvent) {
        let _ = self.events.send(event);
    }

    /// Whether a turn has published its token (is actually running).
    fn turn_in_flight(&self) -> bool {
        self.turn_cancel
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    /// Stop admitting input so the session can be closed. Returns `false`, and
    /// changes nothing, when admitted work is running or waiting: such a
    /// session must not be closed. Once this returns `true`, every later
    /// [`submit`](Self::submit) is refused as [`Admit::Closed`].
    pub fn close_admission(&self) -> bool {
        let mut admission = self.admission();
        if admission.running || admission.command || self.turn_in_flight() {
            return false;
        }
        admission.closed = true;
        true
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

/// Exclusive use of a session for one command; see
/// [`SessionHost::reserve_command`].
pub struct CommandReservation<'a> {
    host: &'a SessionHost,
}

impl CommandReservation<'_> {
    /// The session's runtime. No admitted turn can hold it while the
    /// reservation lives; a turn driven outside admission is waited for.
    pub async fn runtime(&self) -> tokio::sync::MutexGuard<'_, SessionRuntime> {
        self.host.runtime.lock().await
    }
}

impl Drop for CommandReservation<'_> {
    fn drop(&mut self) {
        self.host.admission().command = false;
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
