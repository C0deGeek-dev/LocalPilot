//! Sharing the full-screen session with paired devices (`/remote share`).
//!
//! The session stays in this process. [`RemoteShare`] connects its feed to the
//! workspace server as a relay (see `relay_client`), reconnects when the
//! connection drops, and is the one place the terminal decides whether remote
//! work may still be accepted:
//!
//! - Every share has a fence the terminal clears synchronously (stop, a new
//!   session, exit). A request the host has not yet accepted is refused the
//!   moment the fence is down, whatever the relay is doing.
//! - Every connection attempt has its own fence, cleared when that connection
//!   ends, so nothing delivered over an old connection is accepted later.
//! - The runtime's admission gate must still say the session is owned.
//!
//! Requests from devices arrive on one queue that lives as long as the
//! terminal; the terminal drains it in its own loop, between keystrokes, so the
//! runtime keeps a single owner. Reports from a relay task carry the share they
//! belong to; a report from an earlier share is ignored.

#![cfg_attr(not(feature = "tui"), allow(dead_code))]

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use localpilot_core::admission::AdmissionGate;
use localpilot_core::SessionId;
use localpilot_rpc::AskRegistry;
use localpilot_server::{Conn, TransportError};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::relay_client::{run_relay, RelayEnd, RelayIdentity};
use crate::relay_host::{Open, SourceHost, SourceRequest, Standing};
use crate::server_feed::SessionFeed;

/// Requests waiting for the terminal to take them.
const REQUEST_QUEUE: usize = 64;
/// Retry delays after a dropped or failed connection; after the last, every
/// [`STEADY_RETRY`] (with jitter) while the server answers.
const LADDER: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
];
const STEADY_RETRY: Duration = Duration::from_secs(30);
/// How often a shared session checks that it still owns the session.
const OWNER_POLL: Duration = Duration::from_millis(250);
/// Why sharing ended when the terminal stopped owning its session.
const OWNER_LOST: &str = "Sharing ended: this terminal no longer owns the session.";

/// Resolves once `gate` no longer says the session is owned (checked every
/// [`OWNER_POLL`]).
async fn owner_lost(gate: &AdmissionGate) {
    loop {
        if !gate.is_owned() {
            return;
        }
        tokio::time::sleep(OWNER_POLL).await;
    }
}

/// Where sharing stands, for the header and `/remote`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShareState {
    Off,
    Connecting,
    Shared,
    /// The connection dropped; the next attempt is due in `retry_in`.
    Interrupted {
        retry_in: Duration,
    },
}

/// A change a relay task reports, for the terminal to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShareNotice {
    Shared,
    Restored,
    Interrupted {
        retry_in: Duration,
    },
    /// Sharing ended on its own; the text says why.
    Ended(String),
    /// The share's first attendance: nobody attached yet.
    NoDevicesYet,
    Attached {
        name: String,
        scope: Option<localpilot_rpc::Scope>,
    },
    Detached {
        name: String,
    },
    /// An attached device is no longer allowed (it is being disconnected).
    NotAllowed {
        name: String,
        standing: localpilot_rpc::DeviceStanding,
    },
    /// More arrivals and departures than one update shows one by one.
    Summary {
        attached: usize,
        detached: usize,
    },
    /// Devices that attached and left again between two updates (both shown
    /// by the counts and absent from both lists).
    CameAndWent(u64),
    /// Arrivals and departures counted while the list could not say which
    /// devices (one of the two lists is unknown).
    Activity {
        arrived: u64,
        left: u64,
    },
    /// Notices dropped because too many were waiting to be shown.
    Overflow(usize),
}

/// Per-device notices one attendance update shows; beyond that, a summary.
const DEVICE_NOTICES: usize = 3;
/// Notices kept until shown; beyond that they are counted, not kept.
const NOTICES_KEPT: usize = 16;

/// Keep `notice` for the terminal, or count it once too many are waiting.
fn push_notice(notices: &mut Vec<ShareNotice>, notice: ShareNotice) {
    if notices.len() + 1 < NOTICES_KEPT {
        notices.push(notice);
        return;
    }
    match notices.last_mut() {
        Some(ShareNotice::Overflow(dropped)) => *dropped += 1,
        _ => notices.push(ShareNotice::Overflow(1)),
    }
}

/// A device's name for a notice.
fn label(device: &localpilot_rpc::AttendedDevice) -> String {
    device
        .name
        .clone()
        .unwrap_or_else(|| "a paired device".to_string())
}

/// A connection to the server, of whatever transport.
pub(crate) trait Link: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Link for T {}

/// One dial of the server.
pub(crate) type Dialing =
    Pin<Box<dyn Future<Output = Result<Box<dyn Link>, TransportError>> + Send>>;

/// How a share connects to its server. Production dials the workspace's own
/// server (the one the gateway discovers); tests dial a test server.
pub(crate) type Dial = Arc<dyn Fn() -> Dialing + Send + Sync>;

/// How a share waits before a retry (tests shorten it; the delay it is asked
/// for is what the terminal is told).
pub(crate) type Nap =
    Arc<dyn Fn(Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// A device's display name by id, if the registry knows it.
pub(crate) type Names = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The session a share publishes, captured on the terminal's thread while the
/// runtime is not borrowed.
#[derive(Clone)]
pub(crate) struct Tracked {
    pub(crate) session: SessionId,
    /// `None` when the runtime holds no claim (an incognito session).
    pub(crate) claim: Option<(u64, String)>,
    pub(crate) gate: AdmissionGate,
}

struct Current {
    share: u64,
    task: Option<tokio::task::JoinHandle<()>>,
    /// The share's fence: down once the terminal stops it.
    fence: Arc<AtomicBool>,
    stop: CancellationToken,
    session: SessionId,
}

struct Inner {
    state: ShareState,
    /// Shares started so far; a report is for the current one only.
    shares: u64,
    current: Option<Current>,
    tracked: Option<Tracked>,
    notices: Vec<ShareNotice>,
    /// Who is attached, as the current connection's last accepted update
    /// says; `None` while unknown (not connected, or no update yet).
    attendance: Option<crate::relay_host::Telemetry>,
    /// The task of the share that ended last, for a bounded wait at exit.
    ending: Option<tokio::task::JoinHandle<()>>,
    /// When the interrupted share's next attempt is due: set exactly when
    /// the wait is scheduled, cleared on any other state, under this lock.
    retry_at: Option<std::time::Instant>,
}

/// One terminal's sharing. See the module docs.
pub(crate) struct RemoteShare {
    inner: Arc<Mutex<Inner>>,
    requests: mpsc::Sender<SourceRequest>,
    incoming: Mutex<mpsc::Receiver<SourceRequest>>,
    dial: Dial,
    standing: Standing,
    names: Names,
    source: Arc<crate::relay_source::SessionSource>,
    nap: Nap,
}

/// What `/remote share` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShareOutcome {
    Connecting,
    AlreadyShared,
    AlreadyConnecting,
    Refused(String),
}

/// What `/remote stop` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StopOutcome {
    Stopped,
    Cancelled,
    NotShared,
}

impl RemoteShare {
    /// Sharing for the workspace at `cwd`: its own server, and the per-user
    /// device registry read before every decision (a read error denies).
    pub(crate) fn for_workspace(
        cwd: &Path,
        source: Arc<crate::relay_source::SessionSource>,
    ) -> Self {
        let root = cwd.to_path_buf();
        let dial: Dial = Arc::new(move || {
            let root = root.clone();
            Box::pin(async move {
                let endpoint = localpilot_server::Endpoint::for_workspace(&root)?;
                let conn: Conn = localpilot_server::connect(&endpoint).await?;
                Ok(Box::new(conn) as Box<dyn Link>)
            })
        });
        let registry = Arc::new(crate::server_devices::DeviceRegistry::for_user(cwd));
        let standing: Standing = {
            let registry = registry.clone();
            Arc::new(move |device: &str| registry.standing(device).ok().map(|s| s.scope))
        };
        let names = Arc::new(move |device: &str| {
            registry
                .list()
                .ok()?
                .into_iter()
                .find(|record| record.id == device)
                .map(|record| record.name)
        });
        Self::new(dial, standing, names, source)
    }

    pub(crate) fn new(
        dial: Dial,
        standing: Standing,
        names: Names,
        source: Arc<crate::relay_source::SessionSource>,
    ) -> Self {
        let (requests, incoming) = mpsc::channel(REQUEST_QUEUE);
        Self {
            inner: Arc::new(Mutex::new(Inner {
                state: ShareState::Off,
                shares: 0,
                current: None,
                tracked: None,
                notices: Vec::new(),
                attendance: None,
                retry_at: None,
                ending: None,
            })),
            requests,
            incoming: Mutex::new(incoming),
            dial,
            standing,
            names,
            source,
            nap: Arc::new(|delay| Box::pin(tokio::time::sleep(delay))),
        }
    }

    /// The same sharing, waiting with `nap` before each retry.
    #[cfg(test)]
    pub(crate) fn with_nap(mut self, nap: Nap) -> Self {
        self.nap = nap;
        self
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record the session the terminal runs now (called while the runtime is
    /// not borrowed). A different session ends the old one's share first.
    /// Returns whether a share was ended.
    pub(crate) fn track(&self, tracked: Tracked) -> bool {
        let mut inner = self.inner();
        let ended = inner
            .current
            .as_ref()
            .is_some_and(|current| current.session != tracked.session);
        if ended {
            end_current(&mut inner);
        }
        inner.tracked = Some(tracked);
        ended
    }

    /// End the share for the session about to be replaced (before a command
    /// that changes the session runs). Returns whether one was ended.
    pub(crate) fn end_for_identity(&self) -> bool {
        let mut inner = self.inner();
        let ended = inner.current.is_some();
        end_current(&mut inner);
        ended
    }

    pub(crate) fn state(&self) -> ShareState {
        self.inner().state.clone()
    }

    /// Whether sharing is on (connecting, shared or reconnecting).
    pub(crate) fn active(&self) -> bool {
        self.inner().current.is_some()
    }

    /// The session the terminal runs now, as last tracked.
    /// How this share reaches its server (pairing dials the same one).
    pub(crate) fn dial(&self) -> Dial {
        self.dial.clone()
    }

    /// When an interrupted share tries again (`None` when not waiting).
    #[cfg(test)]
    pub(crate) fn retry_at(&self) -> Option<std::time::Instant> {
        self.inner().retry_at
    }

    /// The share's state and its retry deadline, read together.
    pub(crate) fn state_and_retry(&self) -> (ShareState, Option<std::time::Instant>) {
        let inner = self.inner();
        (inner.state.clone(), inner.retry_at)
    }

    pub(crate) fn tracked_session(&self) -> Option<SessionId> {
        self.inner().tracked.as_ref().map(|tracked| tracked.session)
    }

    /// The devices attached now, or `None` when that is not known.
    pub(crate) fn devices(&self) -> Option<Vec<localpilot_rpc::AttendedDevice>> {
        self.inner()
            .attendance
            .as_ref()
            .and_then(|telemetry| telemetry.devices.clone())
    }

    /// Notices the relay tasks reported since the last call.
    pub(crate) fn take_notices(&self) -> Vec<ShareNotice> {
        std::mem::take(&mut self.inner().notices)
    }

    /// The next request from a device, if one is waiting.
    pub(crate) fn next_request(&self) -> Option<SourceRequest> {
        self.incoming
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .try_recv()
            .ok()
    }

    /// A device's display name, if the registry knows it.
    pub(crate) fn device_name(&self, device: &str) -> Option<String> {
        (self.names)(device)
    }

    /// The session's source (its feed and ask registry).
    pub(crate) fn source(&self) -> &crate::relay_source::SessionSource {
        &self.source
    }

    /// Wait at most `within` for the last ended share's relay to finish (its
    /// closing notice to the server), so a confirmed exit is orderly but never
    /// held up by an unresponsive server.
    pub(crate) async fn wind_down(&self, within: Duration) {
        let ending = self.inner().ending.take();
        if let Some(ending) = ending {
            let _ = tokio::time::timeout(within, ending).await;
        }
    }

    /// Share the tracked session.
    pub(crate) fn share(&self) -> ShareOutcome {
        let mut inner = self.inner();
        match inner.state {
            ShareState::Shared => return ShareOutcome::AlreadyShared,
            ShareState::Connecting => return ShareOutcome::AlreadyConnecting,
            ShareState::Interrupted { .. } => return ShareOutcome::AlreadyShared,
            ShareState::Off => {}
        }
        let Some(tracked) = inner.tracked.clone() else {
            return ShareOutcome::Refused("the session has not started yet".to_string());
        };
        let Some((owner_generation, holder_instance)) = tracked.claim.clone() else {
            return ShareOutcome::Refused(
                "this session keeps nothing on disk (incognito), so it cannot be shared"
                    .to_string(),
            );
        };
        if !tracked.gate.is_owned() {
            return ShareOutcome::Refused("this terminal no longer owns the session".to_string());
        }
        inner.shares += 1;
        let share = inner.shares;
        let fence = Arc::new(AtomicBool::new(true));
        let stop = CancellationToken::new();
        let feed: Arc<SessionFeed> = self.source.feed();
        let asks: AskRegistry = self.source.asks().clone();
        inner.current = Some(Current {
            share,
            task: None,
            fence: fence.clone(),
            stop: stop.clone(),
            session: tracked.session,
        });
        inner.state = ShareState::Connecting;
        inner.retry_at = None;
        drop(inner);
        let identity = RelayIdentity {
            session: tracked.session,
            owner_generation,
            holder_instance,
            share: share_id(),
        };
        let task = ShareTask {
            nap: self.nap.clone(),
            inner: self.inner.clone(),
            share,
            fence,
            gate: tracked.gate,
            stop,
            dial: self.dial.clone(),
            standing: self.standing.clone(),
            requests: self.requests.clone(),
            asks,
            feed,
            identity,
        };
        let handle = tokio::spawn(task.run());
        if let Some(current) = &mut self.inner().current {
            if current.share == share {
                current.task = Some(handle);
            }
        }
        ShareOutcome::Connecting
    }

    /// Stop sharing: remote work not yet accepted is refused from this
    /// instant; the relay ends without waiting for the server.
    pub(crate) fn stop(&self) -> StopOutcome {
        let mut inner = self.inner();
        let outcome = match inner.state {
            ShareState::Off => return StopOutcome::NotShared,
            ShareState::Connecting => StopOutcome::Cancelled,
            ShareState::Shared | ShareState::Interrupted { .. } => StopOutcome::Stopped,
        };
        end_current(&mut inner);
        outcome
    }
}

/// Clear the share's fence first, then end its task, and forget it.
fn end_current(inner: &mut Inner) {
    inner.attendance = None;
    if let Some(current) = inner.current.take() {
        current.fence.store(false, Ordering::SeqCst);
        current.stop.cancel();
        if current.task.is_some() {
            inner.ending = current.task;
        }
    }
    inner.state = ShareState::Off;
    inner.retry_at = None;
}

/// Take one accepted attendance update for the current connection: keep it,
/// and say what changed (bounded). A baseline starts the connection's view;
/// a share's first baseline says when nobody is attached yet, a reconnect's
/// says nothing (its devices are not new arrivals).
fn apply_attendance(inner: &mut Inner, telemetry: crate::relay_host::Telemetry, reconnect: bool) {
    use localpilot_rpc::DeviceStanding;
    let previous = inner.attendance.take();
    let mut notices = Vec::new();
    match (&previous, telemetry.baseline) {
        (_, true) | (None, _) => {
            if telemetry.baseline
                && !reconnect
                && telemetry.devices.as_ref().is_some_and(Vec::is_empty)
            {
                notices.push(ShareNotice::NoDevicesYet);
            }
        }
        (Some(previous), false) => {
            let arrivals = telemetry.attaches.saturating_sub(previous.attaches);
            let departures = telemetry.detaches.saturating_sub(previous.detaches);
            if let (Some(before), Some(now)) = (&previous.devices, &telemetry.devices) {
                let attached: Vec<_> = now
                    .iter()
                    .filter(|device| !before.iter().any(|old| old.id == device.id))
                    .collect();
                let detached: Vec<_> = before
                    .iter()
                    .filter(|device| !now.iter().any(|new| new.id == device.id))
                    .collect();
                let refused: Vec<_> = now
                    .iter()
                    .filter(|device| device.standing != DeviceStanding::Active)
                    .filter(|device| {
                        before
                            .iter()
                            .find(|old| old.id == device.id)
                            .is_none_or(|old| old.standing != device.standing)
                    })
                    .collect();
                if attached.len() + detached.len() > DEVICE_NOTICES {
                    notices.push(ShareNotice::Summary {
                        attached: attached.len(),
                        detached: detached.len(),
                    });
                } else {
                    for device in &attached {
                        notices.push(ShareNotice::Attached {
                            name: label(device),
                            scope: device.scope,
                        });
                    }
                    for device in &detached {
                        notices.push(ShareNotice::Detached {
                            name: label(device),
                        });
                    }
                }
                for device in refused.iter().take(DEVICE_NOTICES) {
                    notices.push(ShareNotice::NotAllowed {
                        name: label(device),
                        standing: device.standing,
                    });
                }
                // Arrivals and departures the lists do not show: devices that
                // came and went in between (as many as both counts confirm).
                let unseen_arrivals = arrivals.saturating_sub(attached.len() as u64);
                let unseen_departures = departures.saturating_sub(detached.len() as u64);
                let came_and_went = unseen_arrivals.min(unseen_departures);
                if came_and_went > 0 {
                    notices.push(ShareNotice::CameAndWent(came_and_went));
                }
            } else if arrivals > 0 || departures > 0 {
                // One list is unknown: the counts are all there is, and they
                // say nothing about which devices.
                notices.push(ShareNotice::Activity {
                    arrived: arrivals,
                    left: departures,
                });
            }
        }
    }
    for notice in notices {
        push_notice(&mut inner.notices, notice);
    }
    inner.attendance = Some(telemetry);
}

/// A fresh share generation for the server (never reused).
fn share_id() -> String {
    localpilot_core::EventId::new().to_string()
}

/// The delay before retry number `failures` (from zero).
fn retry_delay(failures: usize) -> Duration {
    LADDER.get(failures).copied().unwrap_or_else(|| {
        // +/-20% jitter, from the clock (no randomness source is needed for
        // spreading reconnects).
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let spread = f64::from(nanos % 1000) / 1000.0 * 0.4 - 0.2;
        STEADY_RETRY.mul_f64(1.0 + spread)
    })
}

struct ShareTask {
    nap: Nap,
    inner: Arc<Mutex<Inner>>,
    share: u64,
    fence: Arc<AtomicBool>,
    gate: AdmissionGate,
    stop: CancellationToken,
    dial: Dial,
    standing: Standing,
    requests: mpsc::Sender<SourceRequest>,
    asks: AskRegistry,
    feed: Arc<SessionFeed>,
    identity: RelayIdentity,
}

/// How one connection attempt ended.
enum Attempt {
    /// The terminal stopped sharing: nothing to report.
    Stopped,
    /// Sharing is over for a reason the user is told.
    Over(String),
    /// The server is not running.
    Absent,
    /// The connection failed or dropped; worth retrying.
    Dropped,
}

impl ShareTask {
    /// Report for this share only, while it is still the current one.
    fn report(&self, state: Option<ShareState>, notice: Option<ShareNotice>) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.current.as_ref().map(|c| c.share) != Some(self.share) {
            return false;
        }
        if let Some(state) = state {
            inner.retry_at = match &state {
                ShareState::Interrupted { retry_in } => Some(std::time::Instant::now() + *retry_in),
                _ => None,
            };
            inner.state = state;
        }
        if let Some(notice) = notice {
            push_notice(&mut inner.notices, notice);
        }
        true
    }

    /// Sharing ended on its own: forget it (if still current) and say why.
    fn over(&self, why: String) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if inner.current.as_ref().map(|c| c.share) == Some(self.share) {
            end_current(&mut inner);
            push_notice(&mut inner.notices, ShareNotice::Ended(why));
        }
    }

    async fn run(self) {
        let mut failures = 0usize;
        let mut ever_shared = false;
        loop {
            if self.stop.is_cancelled() {
                return;
            }
            match self.attempt(ever_shared).await {
                (Attempt::Stopped, _) => return,
                (Attempt::Over(why), _) => return self.over(why),
                (outcome, shared) => {
                    if shared {
                        ever_shared = true;
                        failures = 0;
                    }
                    let absent = matches!(outcome, Attempt::Absent);
                    // A server that is not running gets the short ladder
                    // only (enough to ride a restart), and none at all when
                    // this share never connected.
                    if absent && (!ever_shared || failures >= LADDER.len()) {
                        return self.over(
                            "Sharing needs a background server for this workspace. Start it with `localpilot serve`, then /remote share."
                                .to_string(),
                        );
                    }
                    let delay = retry_delay(failures);
                    failures += 1;
                    if !self.report(
                        Some(ShareState::Interrupted { retry_in: delay }),
                        Some(ShareNotice::Interrupted { retry_in: delay }),
                    ) {
                        return;
                    }
                    tokio::select! {
                        () = self.stop.cancelled() => return,
                        // Ownership can end while waiting to retry.
                        () = owner_lost(&self.gate) => return self.over(OWNER_LOST.to_string()),
                        () = (self.nap)(delay) => {}
                    }
                    if !self.report(Some(ShareState::Connecting), None) {
                        return;
                    }
                }
            }
        }
    }

    /// One connection: register, publish until it ends. Returns how it ended
    /// and whether it reached the shared state.
    async fn attempt(&self, reconnect: bool) -> (Attempt, bool) {
        let conn = tokio::select! {
            () = self.stop.cancelled() => return (Attempt::Stopped, false),
            // Ownership can end while the dial is pending.
            () = owner_lost(&self.gate) => return (Attempt::Over(OWNER_LOST.to_string()), false),
            conn = (self.dial)() => conn,
        };
        let conn = match conn {
            Ok(conn) => conn,
            Err(TransportError::NotRunning) => return (Attempt::Absent, false),
            Err(_) => return (Attempt::Dropped, false),
        };
        // This connection's own fence: down once it ends, so a request it
        // delivered is never accepted after.
        let attempt = Arc::new(AtomicBool::new(true));
        let open: Open = {
            let fence = self.fence.clone();
            let attempt = attempt.clone();
            let gate = self.gate.clone();
            Arc::new(move || {
                fence.load(Ordering::SeqCst) && attempt.load(Ordering::SeqCst) && gate.is_owned()
            })
        };
        // Attendance from this connection only, while it and its share are
        // current.
        let on_attendance: crate::relay_host::OnAttendance = {
            let inner = self.inner.clone();
            let share = self.share;
            let attempt = attempt.clone();
            Arc::new(move |telemetry| {
                if !attempt.load(Ordering::SeqCst) {
                    return;
                }
                let mut inner = inner.lock().unwrap_or_else(PoisonError::into_inner);
                if inner.current.as_ref().map(|c| c.share) != Some(share) {
                    return;
                }
                apply_attendance(&mut inner, telemetry, reconnect);
            })
        };
        let host = SourceHost {
            requests: self.requests.clone(),
            standing: self.standing.clone(),
            open,
            attendance: Some(on_attendance),
            asks: self.asks.clone(),
        };
        let relay_stop = self.stop.child_token();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let relay = run_relay(
            conn,
            self.feed.clone(),
            &self.identity,
            &relay_stop,
            Some(host),
            Some(ready_tx),
        );
        tokio::pin!(relay);
        let mut ready_rx = Some(ready_rx);
        let mut shared = false;
        let mut owner_lost = false;
        let mut poll = tokio::time::interval(OWNER_POLL);
        let end = loop {
            tokio::select! {
                end = &mut relay => break end,
                ready = async {
                    match ready_rx.as_mut() {
                        Some(rx) => rx.await,
                        None => std::future::pending().await,
                    }
                } => {
                    ready_rx = None;
                    if ready.is_ok() {
                        shared = true;
                        let notice = if reconnect { ShareNotice::Restored } else { ShareNotice::Shared };
                        if !self.report(Some(ShareState::Shared), Some(notice)) {
                            relay_stop.cancel();
                        }
                    }
                }
                _ = poll.tick() => {
                    if !self.gate.is_owned() {
                        // Refuse new work now; then end the relay.
                        attempt.store(false, Ordering::SeqCst);
                        owner_lost = true;
                        relay_stop.cancel();
                    }
                }
            }
        };
        attempt.store(false, Ordering::SeqCst);
        // Who was attached over this connection is not known any more.
        {
            let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            if inner.current.as_ref().map(|c| c.share) == Some(self.share) {
                inner.attendance = None;
            }
        }
        if owner_lost {
            return (Attempt::Over(OWNER_LOST.to_string()), shared);
        }
        let outcome = match end {
            Ok(RelayEnd::Stopped) => Attempt::Stopped,
            Ok(RelayEnd::ServerGone) | Err(_) => Attempt::Dropped,
            Ok(RelayEnd::Refused(reason)) => {
                Attempt::Over(format!("Could not share this session. {reason}"))
            }
            Ok(RelayEnd::SessionEnded) => Attempt::Over("Sharing ended: the session ended.".to_string()),
            Ok(RelayEnd::TooLarge) => Attempt::Over(
                "Sharing ended: something in this session is too large to share.".to_string(),
            ),
            Ok(RelayEnd::Unstable) => Attempt::Over(
                "Sharing ended: the connection kept falling behind. Use /remote share to try again."
                    .to_string(),
            ),
        };
        (outcome, shared)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! A test's stand-in for the server end of a relay, and dials it controls.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::atomic::{AtomicU8, AtomicUsize};

    use localpilot_rpc::{
        ClientCommand, ClientRecord, InputDisposition, Scope, ServerEvent, ServerRecord,
        RPC_PROTOCOL_VERSION,
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::*;

    pub(crate) const QUICK: Duration = Duration::from_secs(5);

    /// The far end of one relay connection, as a test plays the server.
    pub(crate) struct Server {
        pub(crate) lines: mpsc::UnboundedReceiver<serde_json::Value>,
        write: tokio::io::WriteHalf<tokio::io::DuplexStream>,
        share: String,
        reader: tokio::task::JoinHandle<()>,
    }

    impl Drop for Server {
        /// Dropping the server closes its end of the connection (the reader
        /// holds the other half).
        fn drop(&mut self) {
            self.reader.abort();
        }
    }

    impl Server {
        /// Take the registration, then read every later line in the background.
        pub(crate) async fn accept(peer: tokio::io::DuplexStream) -> Self {
            let (read, write) = tokio::io::split(peer);
            let mut read = BufReader::new(read);
            let mut line = String::new();
            read.read_line(&mut line).await.unwrap();
            let record: ClientRecord = serde_json::from_str(&line).unwrap();
            let ClientCommand::RelayHost { share, .. } = record.command else {
                panic!("expected a registration: {line}");
            };
            let (tx, lines) = mpsc::unbounded_channel();
            let reader = tokio::spawn(async move {
                loop {
                    let mut line = String::new();
                    match read.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            if let Ok(value) = serde_json::from_str(&line) {
                                let _ = tx.send(value);
                            }
                        }
                    }
                }
            });
            Self {
                lines,
                write,
                share,
                reader,
            }
        }

        pub(crate) async fn send(&mut self, event: ServerEvent) {
            let mut bytes = serde_json::to_vec(&ServerRecord {
                v: RPC_PROTOCOL_VERSION,
                id: None,
                seq: None,
                event,
            })
            .unwrap();
            bytes.push(b'\n');
            self.write.write_all(&bytes).await.unwrap();
        }

        /// Tell the source who is attached.
        pub(crate) async fn devices(
            &mut self,
            version: u64,
            baseline: bool,
            counts: (u64, u64),
            devices: Option<Vec<localpilot_rpc::AttendedDevice>>,
        ) {
            let share = self.share.clone();
            self.send_for(share, version, baseline, counts, devices)
                .await;
        }

        /// The same, naming any share.
        pub(crate) async fn send_for(
            &mut self,
            share: String,
            version: u64,
            baseline: bool,
            (attaches, detaches): (u64, u64),
            devices: Option<Vec<localpilot_rpc::AttendedDevice>>,
        ) {
            self.send(ServerEvent::RelayDevices {
                relay: "r".to_string(),
                share,
                version,
                baseline,
                attaches,
                detaches,
                devices,
            })
            .await;
        }

        pub(crate) async fn ready(&mut self) {
            self.send(ServerEvent::RelayReady {
                relay: "r".to_string(),
            })
            .await;
        }

        pub(crate) async fn command(&mut self, device: &str, n: u64, command: ClientCommand) {
            let share = self.share.clone();
            self.send(ServerEvent::RelayCommand {
                relay: "r".to_string(),
                share,
                device: device.to_string(),
                n,
                command: Box::new(command),
            })
            .await;
        }

        /// The next record of `kind` the source sends, or `None` once the
        /// connection ended.
        pub(crate) async fn next_of(&mut self, kind: &str) -> Option<serde_json::Value> {
            tokio::time::timeout(QUICK, async {
                loop {
                    let line = self.lines.recv().await?;
                    if line.get("type").and_then(|t| t.as_str()) == Some(kind) {
                        return Some(line);
                    }
                }
            })
            .await
            .expect("in time")
        }
    }

    pub(crate) fn prompt(text: &str) -> ClientCommand {
        ClientCommand::Prompt {
            text: text.to_string(),
            disposition: InputDisposition::default(),
        }
    }

    /// An attached device for a test's attendance record.
    pub(crate) fn device(
        id: &str,
        standing: localpilot_rpc::DeviceStanding,
    ) -> localpilot_rpc::AttendedDevice {
        localpilot_rpc::AttendedDevice {
            id: id.to_string(),
            name: Some(format!("{id}-phone")),
            scope: Some(Scope::Control),
            standing,
        }
    }

    /// A prompt sent as a follow-up (queued while a turn runs).
    pub(crate) fn follow_up(text: &str) -> ClientCommand {
        ClientCommand::Prompt {
            text: text.to_string(),
            disposition: InputDisposition::FollowUp,
        }
    }

    /// A prompt sent to steer the running turn.
    pub(crate) fn steer(text: &str) -> ClientCommand {
        ClientCommand::Prompt {
            text: text.to_string(),
            disposition: InputDisposition::Steer,
        }
    }

    type Queued = Result<tokio::io::DuplexStream, TransportError>;

    /// Dials handed out in order: each test pushes what the next dial gets
    /// (an empty queue finds no server).
    #[derive(Clone, Default)]
    pub(crate) struct Dials {
        queue: Arc<Mutex<std::collections::VecDeque<Queued>>>,
        count: Arc<AtomicUsize>,
    }

    impl Dials {
        pub(crate) fn push_link(&self) -> tokio::io::DuplexStream {
            let (client, peer) = tokio::io::duplex(256 * 1024);
            self.queue.lock().unwrap().push_back(Ok(client));
            peer
        }

        pub(crate) fn push_error(&self, error: TransportError) {
            self.queue.lock().unwrap().push_back(Err(error));
        }

        pub(crate) fn dial(&self) -> Dial {
            let dials = self.clone();
            Arc::new(move || {
                let dials = dials.clone();
                Box::pin(async move {
                    dials.count.fetch_add(1, Ordering::SeqCst);
                    let next = dials.queue.lock().unwrap().pop_front();
                    match next {
                        Some(Ok(link)) => Ok(Box::new(link) as Box<dyn Link>),
                        Some(Err(error)) => Err(error),
                        None => Err(TransportError::NotRunning),
                    }
                })
            })
        }

        pub(crate) fn count(&self) -> usize {
            self.count.load(Ordering::SeqCst)
        }
    }

    /// A device standing a test can change: 0 revoked, 1 observe, 2 control.
    pub(crate) fn standing(level: &Arc<AtomicU8>) -> Standing {
        let level = level.clone();
        Arc::new(move |_| match level.load(Ordering::SeqCst) {
            1 => Some(Scope::Observe),
            2 => Some(Scope::Control),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::atomic::AtomicU8;

    use localpilot_core::admission::Admission;
    use localpilot_rpc::RelayRequestState;

    use super::test_support::{prompt, standing, Dials, Server, QUICK};
    use super::*;
    use crate::relay_source::SessionSource;

    struct Fixture {
        share: RemoteShare,
        dials: Dials,
        level: Arc<AtomicU8>,
        gate: AdmissionGate,
        session: SessionId,
    }

    fn fixture_with(dial: Option<Dial>) -> Fixture {
        fixture_napping(
            dial,
            Arc::new(|delay| Box::pin(tokio::time::sleep(delay / 1000))),
        )
    }

    /// A retry wait that never ends on its own (only stopping or losing
    /// ownership ends it).
    fn held_nap() -> Nap {
        Arc::new(|_| Box::pin(std::future::pending::<()>()))
    }

    /// A dial that never completes.
    fn held_dial() -> Dial {
        Arc::new(|| Box::pin(std::future::pending()))
    }

    fn fixture_napping(dial: Option<Dial>, nap: Nap) -> Fixture {
        let dials = Dials::default();
        let level = Arc::new(AtomicU8::new(2));
        let source = Arc::new(SessionSource::start(AskRegistry::default()));
        let share = RemoteShare::new(
            dial.unwrap_or_else(|| dials.dial()),
            standing(&level),
            Arc::new(|_| Some("Pixel".to_string())),
            source,
        )
        .with_nap(nap);
        let gate = AdmissionGate::default();
        let session = SessionId::new();
        share.track(Tracked {
            session,
            claim: Some((1, "me".to_string())),
            gate: gate.clone(),
        });
        Fixture {
            share,
            dials,
            level,
            gate,
            session,
        }
    }

    fn fixture() -> Fixture {
        fixture_with(None)
    }

    async fn until(mut done: impl FnMut() -> bool) {
        tokio::time::timeout(QUICK, async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("in time");
    }

    async fn request(share: &RemoteShare) -> SourceRequest {
        let mut taken = None;
        until(|| {
            taken = share.next_request();
            taken.is_some()
        })
        .await;
        taken.unwrap()
    }

    /// Share through a link the test plays the server of, until shared.
    async fn shared(f: &Fixture) -> Server {
        let peer = f.dials.push_link();
        assert_eq!(f.share.share(), ShareOutcome::Connecting);
        let mut server = Server::accept(peer).await;
        assert_eq!(
            f.share.state(),
            ShareState::Connecting,
            "not shared before the server accepts"
        );
        server.ready().await;
        until(|| f.share.state() == ShareState::Shared).await;
        server
    }

    #[tokio::test]
    async fn a_session_is_shared_only_once_the_server_accepts_it() {
        let f = fixture();
        let _server = shared(&f).await;
        assert_eq!(f.share.take_notices(), vec![ShareNotice::Shared]);
        assert_eq!(f.share.share(), ShareOutcome::AlreadyShared);
        assert_eq!(f.dials.count(), 1, "a second share never dials again");
    }

    #[tokio::test]
    async fn stopping_refuses_a_delivered_request_at_once_but_not_accepted_work() {
        let f = fixture();
        let mut server = shared(&f).await;
        server.command("d1", 1, prompt("first")).await;
        server.command("d1", 2, prompt("second")).await;
        let first = request(&f.share).await;
        let second = request(&f.share).await;
        assert!(first.ticket.accept_now() && second.ticket.accept_now());
        // The host accepts the first, then the share stops before it decides
        // the second.
        first.ticket.report(RelayRequestState::Accepted);
        assert_eq!(f.share.stop(), StopOutcome::Stopped);
        assert!(
            !second.ticket.accept_now(),
            "refused from the instant of stop"
        );
        assert!(first.ticket.may_start(), "accepted work survives stop");
        assert_eq!(f.share.state(), ShareState::Off);
    }

    #[tokio::test]
    async fn a_device_that_lost_its_standing_cannot_start_accepted_work() {
        let f = fixture();
        let mut server = shared(&f).await;
        server.command("d1", 1, prompt("queued")).await;
        let queued = request(&f.share).await;
        assert!(queued.ticket.accept_now());
        f.level.store(1, Ordering::SeqCst); // downgraded to observe
        assert!(!queued.ticket.may_start());
        f.level.store(0, Ordering::SeqCst); // revoked
        assert!(!queued.ticket.may_start());
    }

    #[tokio::test]
    async fn a_report_from_an_earlier_share_never_changes_the_current_one() {
        // The first share's dial is held until after it was stopped and a
        // second share is live.
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        let held = Arc::new(Mutex::new(Some(held)));
        let (first_link, first_peer) = tokio::io::duplex(64 * 1024);
        let first_link = Arc::new(Mutex::new(Some(first_link)));
        let dials = Dials::default();
        let later = dials.clone();
        let called = Arc::new(AtomicBool::new(false));
        let first_called = called.clone();
        let dial: Dial = Arc::new(move || {
            first_called.store(true, Ordering::SeqCst);
            let held = held.lock().unwrap().take();
            let first_link = first_link.lock().unwrap().take();
            let later = later.clone();
            Box::pin(async move {
                if let (Some(held), Some(link)) = (held, first_link) {
                    let _ = held.await;
                    return Ok(Box::new(link) as Box<dyn Link>);
                }
                (later.dial())().await
            })
        });
        let mut f = fixture_with(Some(dial));
        f.dials = dials;
        assert_eq!(f.share.share(), ShareOutcome::Connecting);
        // The first share is dialing (held) when it is stopped.
        until(|| called.load(Ordering::SeqCst)).await;
        assert_eq!(f.share.stop(), StopOutcome::Cancelled);
        let _server = shared(&f).await;
        // The first dial completes now, after its share was stopped: it never
        // registers, and nothing it does changes the current share.
        // Stopping abandoned the pending dial: nothing waits for it any more.
        assert!(release.send(()).is_err());
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(first_peer);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(f.share.state(), ShareState::Shared);
        assert_eq!(f.share.take_notices(), vec![ShareNotice::Shared]);
    }

    #[tokio::test]
    async fn no_server_before_sharing_ever_connected_ends_with_how_to_start_one() {
        let f = fixture();
        f.dials.push_error(TransportError::NotRunning);
        assert_eq!(f.share.share(), ShareOutcome::Connecting);
        until(|| f.share.state() == ShareState::Off).await;
        let notices = f.share.take_notices();
        assert!(
            matches!(notices.as_slice(), [ShareNotice::Ended(why)] if why.contains("localpilot serve")),
            "{notices:?}"
        );
        assert_eq!(f.dials.count(), 1, "no retries against an absent server");
    }

    #[tokio::test]
    async fn a_failing_connection_retries_on_the_ladder_then_steadily() {
        let f = fixture();
        for _ in 0..8 {
            f.dials.push_error(TransportError::Busy);
        }
        assert_eq!(f.share.share(), ShareOutcome::Connecting);
        let mut delays = Vec::new();
        while delays.len() < 6 {
            tokio::time::sleep(Duration::from_millis(1)).await;
            for notice in f.share.take_notices() {
                if let ShareNotice::Interrupted { retry_in } = notice {
                    delays.push(retry_in);
                }
            }
        }
        assert_eq!(
            &delays[..4],
            &[
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(5),
                Duration::from_secs(10)
            ]
        );
        for steady in &delays[4..] {
            assert!(
                *steady >= Duration::from_secs(24) && *steady <= Duration::from_secs(36),
                "{steady:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_server_that_went_away_gets_the_short_ladder_then_the_start_notice() {
        let f = fixture();
        let server = shared(&f).await;
        drop(server); // the server stops; every later dial finds nothing
        let mut delays = Vec::new();
        let ended = loop {
            tokio::time::sleep(Duration::from_millis(1)).await;
            let mut ended = None;
            for notice in f.share.take_notices() {
                match notice {
                    ShareNotice::Interrupted { retry_in } => {
                        delays.push(retry_in);
                    }
                    ShareNotice::Ended(why) => ended = Some(why),
                    _ => {}
                }
            }
            if let Some(why) = ended {
                break why;
            }
        };
        assert!(ended.contains("localpilot serve"), "{ended}");
        assert_eq!(
            delays,
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(5),
                Duration::from_secs(10)
            ]
        );
        assert_eq!(f.dials.count(), 5);
        assert_eq!(f.share.state(), ShareState::Off);
    }

    #[tokio::test]
    async fn a_reconnect_registers_a_fresh_connection_and_says_restored() {
        let f = fixture();
        let server = shared(&f).await;
        let _ = f.share.take_notices();
        let next = f.dials.push_link();
        drop(server);
        let mut again = Server::accept(next).await;
        again.ready().await;
        until(|| f.share.state() == ShareState::Shared).await;
        let notices = f.share.take_notices();
        assert!(
            matches!(
                notices.as_slice(),
                [ShareNotice::Interrupted { .. }, ShareNotice::Restored]
            ),
            "{notices:?}"
        );
    }

    #[tokio::test]
    async fn stopping_cancels_a_pending_retry() {
        // The retry is held until the share stops, so the interrupted state
        // cannot pass unobserved.
        let f = fixture_napping(None, held_nap());
        f.dials.push_error(TransportError::Busy);
        assert_eq!(f.share.share(), ShareOutcome::Connecting);
        until(|| matches!(f.share.state(), ShareState::Interrupted { .. })).await;
        assert_eq!(f.share.stop(), StopOutcome::Stopped);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(f.dials.count(), 1, "no dial after stop");
        assert_eq!(f.share.state(), ShareState::Off);
    }

    /// A nap the test releases one wait at a time.
    fn stepped_nap() -> (
        Nap,
        tokio::sync::mpsc::UnboundedReceiver<tokio::sync::oneshot::Sender<()>>,
    ) {
        let (waits, released) =
            tokio::sync::mpsc::unbounded_channel::<tokio::sync::oneshot::Sender<()>>();
        let nap: Nap = Arc::new(move |_| {
            let (go, wait) = tokio::sync::oneshot::channel::<()>();
            let _ = waits.send(go);
            Box::pin(async move {
                let _ = wait.await;
            })
        });
        (nap, released)
    }

    /// The retry deadline is the publisher's: set when each wait is
    /// scheduled (fresh every time, even for the same delay), cleared on any
    /// other state, and never a count of devices while not shared.
    #[tokio::test]
    async fn a_retry_deadline_is_set_for_each_wait_and_cleared_otherwise() {
        let (nap, mut waits) = stepped_nap();
        let f = fixture_napping(None, nap);
        for _ in 0..3 {
            f.dials.push_error(TransportError::Busy);
        }
        assert_eq!(f.share.share(), ShareOutcome::Connecting);
        let mut last = None;
        for _ in 0..3 {
            let go = tokio::time::timeout(Duration::from_secs(5), waits.recv())
                .await
                .expect("a wait was scheduled")
                .expect("the share is waiting");
            let (state, retry_at) = f.share.state_and_retry();
            let retry_in = match state {
                ShareState::Interrupted { retry_in } => retry_in,
                other => panic!("{other:?}"),
            };
            let retry_at = retry_at.expect("a deadline while interrupted");
            let now = std::time::Instant::now();
            assert!(
                retry_at <= now + retry_in && retry_at + retry_in >= now,
                "within its delay"
            );
            if let Some(last) = last {
                assert!(retry_at > last, "a fresh deadline for each wait");
            }
            last = Some(retry_at);
            assert!(f.share.devices().is_none(), "no count while interrupted");
            let _ = go.send(());
            until(|| f.share.retry_at().is_none() || f.share.retry_at() != Some(retry_at)).await;
        }
        // The next dial finds no server, so sharing may already be over.
        let _ = f.share.stop();
        assert_eq!(f.share.retry_at(), None, "cleared once not waiting");
    }

    #[tokio::test]
    async fn stopping_during_a_wait_clears_its_deadline() {
        let f = fixture_napping(None, held_nap());
        f.dials.push_error(TransportError::Busy);
        assert_eq!(f.share.share(), ShareOutcome::Connecting);
        until(|| f.share.retry_at().is_some()).await;
        assert_eq!(f.share.stop(), StopOutcome::Stopped);
        assert_eq!(f.share.state_and_retry(), (ShareState::Off, None));
    }

    fn ended_for_ownership(notices: &[ShareNotice]) -> bool {
        notices
            .iter()
            .any(|n| matches!(n, ShareNotice::Ended(why) if why.contains("no longer owns")))
    }

    #[tokio::test]
    async fn losing_ownership_while_dialing_ends_sharing() {
        let f = fixture_with(Some(held_dial()));
        assert_eq!(f.share.share(), ShareOutcome::Connecting);
        tokio::time::sleep(Duration::from_millis(20)).await;
        f.gate.set(Admission::Lost);
        until(|| f.share.state() == ShareState::Off).await;
        assert!(ended_for_ownership(&f.share.take_notices()));
    }

    #[tokio::test]
    async fn losing_ownership_while_waiting_to_retry_ends_sharing() {
        let f = fixture_napping(None, held_nap());
        f.dials.push_error(TransportError::Busy);
        assert_eq!(f.share.share(), ShareOutcome::Connecting);
        until(|| matches!(f.share.state(), ShareState::Interrupted { .. })).await;
        f.gate.set(Admission::Lost);
        until(|| f.share.state() == ShareState::Off).await;
        assert!(ended_for_ownership(&f.share.take_notices()));
        assert_eq!(f.dials.count(), 1, "the captured claim was never re-sent");
    }

    #[tokio::test]
    async fn losing_ownership_refuses_work_and_ends_sharing() {
        let f = fixture();
        let mut server = shared(&f).await;
        server.command("d1", 1, prompt("late")).await;
        let pending = request(&f.share).await;
        f.gate.set(Admission::Lost);
        assert!(
            !pending.ticket.accept_now(),
            "refused the moment ownership ends"
        );
        until(|| f.share.state() == ShareState::Off).await;
        let notices = f.share.take_notices();
        assert!(
            notices
                .iter()
                .any(|n| matches!(n, ShareNotice::Ended(why) if why.contains("no longer owns"))),
            "{notices:?}"
        );
        assert!(server.next_of("relay_stop").await.is_some() || server.lines.is_closed());
    }

    #[tokio::test]
    async fn a_session_without_a_claim_or_not_owned_is_never_shared() {
        let f = fixture();
        f.share.track(Tracked {
            session: f.session,
            claim: None,
            gate: f.gate.clone(),
        });
        assert!(matches!(f.share.share(), ShareOutcome::Refused(why) if why.contains("incognito")));
        f.share.track(Tracked {
            session: f.session,
            claim: Some((1, "me".to_string())),
            gate: AdmissionGate::new(Admission::Closed),
        });
        assert!(matches!(f.share.share(), ShareOutcome::Refused(_)));
        assert_eq!(f.dials.count(), 0);
    }

    #[tokio::test]
    async fn another_session_ends_the_share_of_the_previous_one() {
        let f = fixture();
        let mut server = shared(&f).await;
        server.command("d1", 1, prompt("for the old session")).await;
        let pending = request(&f.share).await;
        let ended = f.share.track(Tracked {
            session: SessionId::new(),
            claim: Some((1, "me".to_string())),
            gate: f.gate.clone(),
        });
        assert!(ended);
        assert!(!pending.ticket.accept_now());
        assert_eq!(f.share.state(), ShareState::Off);
    }

    #[tokio::test]
    async fn a_decision_reaches_the_server_on_the_wire() {
        let f = fixture();
        let mut server = shared(&f).await;
        server.command("d1", 1, prompt("refused")).await;
        let pending = request(&f.share).await;
        pending.ticket.report(RelayRequestState::Rejected {
            reason: "no".to_string(),
        });
        let result = server.next_of("relay_result").await.expect("a result");
        assert_eq!(result["n"], 1, "{result}");
        assert!(result.to_string().contains("rejected"), "{result}");
    }

    // ---- attendance ---------------------------------------------------------

    use super::test_support::device;
    use localpilot_rpc::DeviceStanding;

    async fn notices_after(
        f: &Fixture,
        wanted: impl Fn(&[ShareNotice]) -> bool,
    ) -> Vec<ShareNotice> {
        let mut seen = Vec::new();
        tokio::time::timeout(QUICK, async {
            loop {
                seen.extend(f.share.take_notices());
                if wanted(&seen) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("notices in time: {seen:?}"));
        seen
    }

    #[tokio::test]
    async fn attendance_is_taken_in_order_for_the_current_share_only() {
        let f = fixture();
        let mut server = shared(&f).await;
        let _ = f.share.take_notices();
        // Before the baseline nothing is taken.
        server
            .devices(
                2,
                false,
                (1, 0),
                Some(vec![device("a", DeviceStanding::Active)]),
            )
            .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(f.share.devices(), None, "unknown until the baseline");
        server.devices(1, true, (0, 0), Some(Vec::new())).await;
        let seen = notices_after(&f, |n| n.contains(&ShareNotice::NoDevicesYet)).await;
        assert_eq!(seen, vec![ShareNotice::NoDevicesYet]);
        assert_eq!(
            f.share.devices(),
            Some(Vec::new()),
            "an authoritative empty list"
        );
        server
            .devices(
                3,
                false,
                (1, 0),
                Some(vec![device("a", DeviceStanding::Active)]),
            )
            .await;
        let seen = notices_after(&f, |n| !n.is_empty()).await;
        assert!(
            matches!(seen.as_slice(), [ShareNotice::Attached { name, .. }] if name == "a-phone"),
            "{seen:?}"
        );
        // An older version and another share's record change nothing.
        server.devices(2, false, (0, 0), Some(Vec::new())).await;
        server
            .send_for(
                "another-share".to_string(),
                9,
                false,
                (0, 0),
                Some(Vec::new()),
            )
            .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(f.share.devices().map(|d| d.len()), Some(1));
        assert!(f.share.take_notices().is_empty());
    }

    #[tokio::test]
    async fn activity_a_list_cannot_show_is_counted_and_notices_stay_bounded() {
        let f = fixture();
        let mut server = shared(&f).await;
        server.devices(1, true, (0, 0), Some(Vec::new())).await;
        let _ = notices_after(&f, |n| n.contains(&ShareNotice::NoDevicesYet)).await;
        // Two came and went between updates: the list is still empty.
        server.devices(2, false, (2, 2), Some(Vec::new())).await;
        let seen = notices_after(&f, |n| !n.is_empty()).await;
        assert_eq!(seen, vec![ShareNotice::CameAndWent(2)]);
        // Five at once: one summary, not five notices.
        let five: Vec<_> = (0..5)
            .map(|n| device(&format!("d{n}"), DeviceStanding::Active))
            .collect();
        server.devices(3, false, (7, 2), Some(five)).await;
        let seen = notices_after(&f, |n| !n.is_empty()).await;
        assert_eq!(
            seen,
            vec![ShareNotice::Summary {
                attached: 5,
                detached: 0
            }]
        );
        // Many updates nobody looked at: at most the kept number, then a count.
        for v in 0..40u64 {
            let list = if v % 2 == 0 {
                Vec::new()
            } else {
                vec![device("x", DeviceStanding::Active)]
            };
            server
                .devices(4 + v, false, (8 + v, 8 + v), Some(list))
                .await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        let kept = f.share.take_notices();
        assert!(kept.len() <= NOTICES_KEPT, "{}", kept.len());
        assert!(
            matches!(kept.last(), Some(ShareNotice::Overflow(_))),
            "{kept:?}"
        );
    }

    #[tokio::test]
    async fn a_device_no_longer_allowed_is_said_so_and_never_reads_as_active() {
        let f = fixture();
        let mut server = shared(&f).await;
        server
            .devices(
                1,
                true,
                (1, 0),
                Some(vec![device("a", DeviceStanding::Active)]),
            )
            .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = f.share.take_notices();
        server
            .devices(
                2,
                false,
                (1, 0),
                Some(vec![device("a", DeviceStanding::Revoked)]),
            )
            .await;
        let seen = notices_after(&f, |n| !n.is_empty()).await;
        assert!(
            matches!(
                seen.as_slice(),
                [ShareNotice::NotAllowed {
                    standing: DeviceStanding::Revoked,
                    ..
                }]
            ),
            "{seen:?}"
        );
        assert_eq!(
            f.share.devices().unwrap()[0].standing,
            DeviceStanding::Revoked
        );
    }

    #[tokio::test]
    async fn attendance_is_unknown_while_interrupted_and_a_reconnect_baseline_is_quiet() {
        let f = fixture();
        let mut server = shared(&f).await;
        server
            .devices(
                1,
                true,
                (1, 0),
                Some(vec![device("a", DeviceStanding::Active)]),
            )
            .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(f.share.devices().map(|d| d.len()), Some(1));
        let _ = f.share.take_notices();
        let next = f.dials.push_link();
        drop(server);
        until(|| f.share.devices().is_none()).await;
        let mut again = Server::accept(next).await;
        again.ready().await;
        until(|| f.share.state() == ShareState::Shared).await;
        again
            .devices(
                1,
                true,
                (1, 0),
                Some(vec![device("a", DeviceStanding::Active)]),
            )
            .await;
        until(|| f.share.devices().is_some()).await;
        let seen = f.share.take_notices();
        assert!(
            !seen
                .iter()
                .any(|n| matches!(n, ShareNotice::Attached { .. } | ShareNotice::NoDevicesYet)),
            "nobody is new after a reconnect: {seen:?}"
        );
    }

    #[tokio::test]
    async fn counts_seen_while_the_list_is_unknown_are_reported_as_they_are() {
        let f = fixture();
        let mut server = shared(&f).await;
        server.devices(1, true, (0, 0), Some(Vec::new())).await;
        let _ = notices_after(&f, |n| n.contains(&ShareNotice::NoDevicesYet)).await;
        // One arrival while the list is unknown: an arrival, never "left".
        server.devices(2, false, (1, 0), None).await;
        let seen = notices_after(&f, |n| !n.is_empty()).await;
        assert_eq!(
            seen,
            vec![ShareNotice::Activity {
                arrived: 1,
                left: 0
            }]
        );
        assert_eq!(f.share.devices(), None, "unknown, not empty");
        // The list comes back with that device still there: nothing new.
        server
            .devices(
                3,
                false,
                (1, 0),
                Some(vec![device("a", DeviceStanding::Active)]),
            )
            .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(f.share.take_notices().is_empty());
        assert_eq!(f.share.devices().map(|d| d.len()), Some(1));
        // A departure while unknown is still a departure.
        server.devices(4, false, (1, 1), None).await;
        let seen = notices_after(&f, |n| !n.is_empty()).await;
        assert_eq!(
            seen,
            vec![ShareNotice::Activity {
                arrived: 0,
                left: 1
            }]
        );
        // Activity across recovery is counted, not lost.
        server.devices(5, false, (3, 3), Some(Vec::new())).await;
        let seen = notices_after(&f, |n| !n.is_empty()).await;
        assert_eq!(
            seen,
            vec![ShareNotice::Activity {
                arrived: 2,
                left: 2
            }]
        );
        // Between two known lists, only arrivals that also left are "came and
        // went": here two arrived, one is attached, one left.
        server
            .devices(
                6,
                false,
                (5, 4),
                Some(vec![device("b", DeviceStanding::Active)]),
            )
            .await;
        let seen = notices_after(&f, |n| n.len() >= 2).await;
        assert!(seen.contains(&ShareNotice::CameAndWent(1)), "{seen:?}");
        assert!(
            seen.iter()
                .any(|n| matches!(n, ShareNotice::Attached { name, .. } if name == "b-phone")),
            "{seen:?}"
        );
    }
}
