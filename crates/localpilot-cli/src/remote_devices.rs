//! The paired devices, as the full-screen terminal lists and revokes them.
//!
//! All registry work runs on one worker thread, strictly in order, so there
//! is never more than one registry operation in progress and never more than
//! one thread. The terminal keeps at most one read and one revoke outstanding.
//! A revoke is admitted to the registry's write lock only before a deadline
//! carried from the moment it was asked for (otherwise it changes nothing and
//! says so); once admitted it runs to completion, whatever the terminal does
//! meanwhile, for as long as the process lives. The terminal therefore keeps
//! the revoke from the moment it is asked for until the worker's own answer,
//! even after its own deadline (then the result is "unknown" until the answer
//! arrives), and starts no other revoke meanwhile.

#![cfg_attr(not(feature = "tui"), allow(dead_code))]

use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::server_devices::{
    DeviceError, DeviceGrant, DeviceRegistry, DeviceRow, DeviceRows, RevokeOutcome,
};

/// How long a revoke may wait for the registry's write lock.
pub(crate) const ADMISSION: Duration = Duration::from_secs(5);
/// How long the terminal waits for a revoke's answer before saying it is not
/// known yet (the admission time plus a margin).
pub(crate) const ANSWER: Duration = Duration::from_secs(6);
/// How long a read may take before the terminal says it is still reading.
pub(crate) const READ_ANSWER: Duration = Duration::from_secs(3);
/// How long a status check may take before the terminal says it did not
/// finish (it stays the one outstanding check).
pub(crate) const STATUS_ANSWER: Duration = Duration::from_secs(6);
/// Requests and answers waiting at once (the terminal keeps at most one read,
/// one revoke and one status check outstanding).
const QUEUE: usize = 4;

/// How the worker checks remote status for a workspace: `None` when the
/// check was cancelled. Tests replace it.
pub(crate) type StatusCheck = Arc<
    dyn Fn(&std::path::Path, &CancellationToken) -> Option<crate::remote_status::ObservedStatus>
        + Send
        + Sync,
>;

/// The real check: the config, index and registry reads, then the gateway's
/// health check, whose network part a cancel stops.
pub(crate) fn real_status_check() -> StatusCheck {
    Arc::new(|cwd: &std::path::Path, cancel: &CancellationToken| {
        if cancel.is_cancelled() {
            return None;
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        runtime.block_on(async {
            tokio::select! {
                biased;
                () = cancel.cancelled() => None,
                status = crate::remote_gateway::observe_at(cwd) => Some(status),
            }
        })
    })
}

enum Request {
    List {
        seq: u64,
    },
    Revoke {
        op: u64,
        id: String,
        deadline: Instant,
    },
    Status {
        seq: u64,
        cwd: std::path::PathBuf,
        cancel: CancellationToken,
    },
}

enum Reply {
    Listed {
        seq: u64,
        rows: Result<DeviceRows, String>,
    },
    Revoked {
        op: u64,
        result: Result<RevokeOutcome, DeviceError>,
    },
    Status {
        seq: u64,
        status: Option<crate::remote_status::ObservedStatus>,
    },
}

/// The one thread that does registry work, in order.
struct Worker {
    requests: SyncSender<Request>,
    replies: Receiver<Reply>,
}

impl Worker {
    fn start(registry: Arc<DeviceRegistry>, check: StatusCheck) -> std::io::Result<Self> {
        let (requests, incoming) = std::sync::mpsc::sync_channel::<Request>(QUEUE);
        let (outgoing, replies) = std::sync::mpsc::sync_channel::<Reply>(QUEUE);
        // Not joined: a revoke admitted to the lock completes even after the
        // terminal let go of it (until the process ends).
        std::thread::Builder::new()
            .name("localpilot-devices".to_string())
            .spawn(move || {
                for request in incoming {
                    let reply = match request {
                        Request::List { seq } => Reply::Listed {
                            seq,
                            rows: registry.rows().map_err(|error| error.to_string()),
                        },
                        Request::Status { seq, cwd, cancel } => {
                            // A cancelled request skips the check entirely.
                            let status = if cancel.is_cancelled() {
                                None
                            } else {
                                check(&cwd, &cancel)
                            };
                            Reply::Status { seq, status }
                        }
                        Request::Revoke { op, id, deadline } => {
                            let result = registry.revoke_device_within(&id, deadline);
                            if matches!(result, Ok(RevokeOutcome::Revoked(_))) {
                                registry.audit(
                                    "revoked",
                                    Some(&id),
                                    "from the full-screen terminal",
                                );
                            }
                            Reply::Revoked { op, result }
                        }
                    };
                    if outgoing.send(reply).is_err() {
                        return;
                    }
                }
            })?;
        Ok(Self { requests, replies })
    }
}

/// The device list, as last read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Listing {
    Loading,
    /// `stale` when a later read failed (the rows are the last good ones).
    Ready {
        rows: DeviceRows,
        stale: bool,
    },
    Failed(String),
}

/// Where a revoke stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RevokeState {
    /// Asked for; the answer is due within [`ANSWER`].
    Pending,
    /// Not answered in time (or the worker went away): it may still be done.
    Unknown,
    /// The registry's own answer.
    Done(Result<RevokeOutcome, String>),
    /// The registry was busy: nothing changed.
    Busy,
}

/// One revoke, kept from the moment it is asked for until its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RevokeOp {
    pub(crate) op: u64,
    /// The device, as it was shown when the revoke was confirmed.
    pub(crate) row: DeviceRow,
    pub(crate) state: RevokeState,
    asked: Instant,
    answered: bool,
}

impl RevokeOp {
    /// Whether the registry may still change because of it.
    pub(crate) fn in_flight(&self) -> bool {
        !self.answered
    }
}

/// Why a revoke was not started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    /// Another revoke may still change the registry.
    InFlight,
    /// The last revoke's answer has not been told yet.
    Untold,
    /// The list is not known to be current since the last revoke (read it
    /// again first): a revoke is only ever aimed from a current list.
    NotCurrent,
    /// The device is revoked already, or its id is not the registry's own.
    NotRevocable,
    /// The worker cannot take requests.
    Unavailable,
}

/// The terminal's device list and its revokes.
pub(crate) struct Devices {
    registry: Arc<DeviceRegistry>,
    worker: Option<Worker>,
    listing: Listing,
    /// The newest read asked for, and the newest applied.
    asked_seq: u64,
    applied_seq: u64,
    /// Reads asked for before this were asked before the last revoke was
    /// answered: they never replace what followed it.
    fence: u64,
    read_pending: bool,
    read_asked: Option<Instant>,
    /// Read again once the read on its way (asked before the last answer)
    /// returns: still one read at a time.
    reread: bool,
    next_op: u64,
    revoke: Option<RevokeOp>,
    admission: Duration,
    answer: Duration,
    check: StatusCheck,
    status_answer: Duration,
    /// How many times the registry changed (or may have) from here: a
    /// revoke accepted, a device paired.
    epoch: u64,
    status_seq: u64,
    /// The one status check outstanding.
    status: Option<StatusAsk>,
    /// A canvas that asked while an older check (for another canvas) was
    /// outstanding: it gets exactly one check once that one answers.
    status_wanted: Option<(u64, std::path::PathBuf)>,
    /// The answer for a canvas, until it takes it.
    status_ready: Option<(u64, Option<crate::remote_status_model::Probe>)>,
}

/// An outstanding status check.
struct StatusAsk {
    seq: u64,
    /// The canvas it is for.
    generation: u64,
    epoch: u64,
    cancel: CancellationToken,
    asked: Instant,
}

/// What asking for a status check did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatusAsked {
    Sent,
    /// One for this canvas is already outstanding.
    Coalesced,
    /// One for another canvas is outstanding; this one follows it.
    Queued,
    Unavailable,
}

impl Devices {
    pub(crate) fn new(registry: Arc<DeviceRegistry>) -> Self {
        Self {
            registry,
            worker: None,
            listing: Listing::Loading,
            asked_seq: 0,
            applied_seq: 0,
            fence: 0,
            read_pending: false,
            read_asked: None,
            reread: false,
            next_op: 0,
            revoke: None,
            admission: ADMISSION,
            answer: ANSWER,
            check: real_status_check(),
            status_answer: STATUS_ANSWER,
            epoch: 0,
            status_seq: 0,
            status: None,
            status_wanted: None,
            status_ready: None,
        }
    }

    /// The same, checking status with `check` (tests).
    #[cfg(test)]
    pub(crate) fn with_check(mut self, check: StatusCheck) -> Self {
        self.check = check;
        self
    }

    /// The same, saying a check is late after `answer` (tests).
    #[cfg(test)]
    pub(crate) fn with_status_answer(mut self, answer: Duration) -> Self {
        self.status_answer = answer;
        self
    }

    /// Tests: the worker takes no more requests (a submit fails).
    #[cfg(test)]
    pub(crate) fn stop_worker_for_test(&mut self) {
        let (requests, _) = std::sync::mpsc::sync_channel(0);
        let (_, replies) = std::sync::mpsc::sync_channel(0);
        self.worker = Some(Worker { requests, replies });
    }

    /// How many times the registry changed (or may have) from here.
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The registry changed (or may have) for a reason this controller does
    /// not see itself (a device paired).
    pub(crate) fn registry_changed(&mut self) {
        self.epoch += 1;
    }

    /// Check remote status for the canvas `generation` (workspace `cwd`).
    pub(crate) fn check_status(&mut self, generation: u64, cwd: &std::path::Path) -> StatusAsked {
        if let Some(pending) = &self.status {
            if pending.generation == generation {
                return StatusAsked::Coalesced;
            }
            self.status_wanted = Some((generation, cwd.to_path_buf()));
            return StatusAsked::Queued;
        }
        let seq = self.status_seq + 1;
        let epoch = self.epoch;
        let cancel = CancellationToken::new();
        let Some(worker) = self.worker() else {
            return StatusAsked::Unavailable;
        };
        let request = Request::Status {
            seq,
            cwd: cwd.to_path_buf(),
            cancel: cancel.clone(),
        };
        if worker.requests.try_send(request).is_err() {
            return StatusAsked::Unavailable;
        }
        self.status_seq = seq;
        self.status = Some(StatusAsk {
            seq,
            generation,
            epoch,
            cancel,
            asked: Instant::now(),
        });
        StatusAsked::Sent
    }

    /// The canvas `generation` went: its check's network part is cancelled
    /// (the check stays owned until the worker answers), and nothing waits
    /// for it any more.
    pub(crate) fn retire_status(&mut self, generation: u64) {
        if let Some(pending) = self.status.as_ref().filter(|p| p.generation == generation) {
            pending.cancel.cancel();
        }
        if self
            .status_wanted
            .as_ref()
            .is_some_and(|(wanted, _)| *wanted == generation)
        {
            self.status_wanted = None;
        }
        if self
            .status_ready
            .as_ref()
            .is_some_and(|(ready, _)| *ready == generation)
        {
            self.status_ready = None;
        }
    }

    /// Whether a check for this canvas is outstanding.
    pub(crate) fn checking_status(&self, generation: u64) -> bool {
        self.status
            .as_ref()
            .is_some_and(|p| p.generation == generation)
            || self
                .status_wanted
                .as_ref()
                .is_some_and(|(wanted, _)| *wanted == generation)
    }

    /// Whether the outstanding check is taking longer than [`STATUS_ANSWER`].
    pub(crate) fn status_late(&self, now: Instant) -> bool {
        self.status
            .as_ref()
            .is_some_and(|pending| now.duration_since(pending.asked) >= self.status_answer)
    }

    /// The answer for canvas `generation`, once (`Some(None)`: it was
    /// cancelled or the worker went away). An answer for any other canvas is
    /// dropped.
    pub(crate) fn take_status(
        &mut self,
        generation: u64,
    ) -> Option<Option<crate::remote_status_model::Probe>> {
        match self.status_ready.take() {
            Some((ready, probe)) if ready == generation => Some(probe),
            _ => None,
        }
    }

    fn status_answered(&mut self, seq: u64, status: Option<crate::remote_status::ObservedStatus>) {
        let Some(pending) = self.status.take_if(|pending| pending.seq == seq) else {
            return;
        };
        let probe = status
            .filter(|_| !pending.cancel.is_cancelled())
            .map(|observed| crate::remote_status_model::Probe {
                observed,
                epoch: pending.epoch,
            });
        // A retired canvas's answer is drained here and dropped.
        if !pending.cancel.is_cancelled() {
            self.status_ready = Some((pending.generation, probe));
        }
        // The canvas that waited behind it gets exactly one check now (or
        // answers unknown when it cannot be started).
        if let Some((generation, cwd)) = self.status_wanted.take() {
            if self.check_status(generation, &cwd) != StatusAsked::Sent {
                self.status_ready = Some((generation, None));
            }
        }
    }

    /// The same, with other time budgets (tests).
    #[cfg(test)]
    pub(crate) fn with_budgets(mut self, admission: Duration, answer: Duration) -> Self {
        self.admission = admission;
        self.answer = answer;
        self
    }

    fn worker(&mut self) -> Option<&Worker> {
        if self.worker.is_none() {
            self.worker = Worker::start(self.registry.clone(), self.check.clone()).ok();
        }
        self.worker.as_ref()
    }

    pub(crate) fn listing(&self) -> &Listing {
        &self.listing
    }

    pub(crate) fn revoke_op(&self) -> Option<&RevokeOp> {
        self.revoke.as_ref()
    }

    /// Whether a read is on its way.
    pub(crate) fn reading(&self) -> bool {
        self.read_pending
    }

    /// Whether the read on its way is taking longer than [`READ_ANSWER`]
    /// (it is still awaited; no other is started).
    pub(crate) fn reading_late(&self, now: Instant) -> bool {
        self.read_pending
            && self
                .read_asked
                .is_some_and(|asked| now.duration_since(asked) >= READ_ANSWER)
    }

    /// Whether the list shown was read after the last revoke's answer (and
    /// its read worked): only then can a revoke be aimed from it.
    pub(crate) fn current(&self) -> bool {
        self.applied_seq >= self.fence
            && matches!(self.listing, Listing::Ready { stale: false, .. })
    }

    /// Read the list again (at most one read at a time: a repeat while one
    /// is on its way does nothing). Returns whether a read is on its way.
    pub(crate) fn refresh(&mut self) -> bool {
        if self.read_pending {
            return true;
        }
        let seq = self.asked_seq + 1;
        let Some(worker) = self.worker() else {
            self.listing = Listing::Failed("the device list cannot be read here".to_string());
            return false;
        };
        match worker.requests.try_send(Request::List { seq }) {
            Ok(()) => {
                self.asked_seq = seq;
                self.read_pending = true;
                self.read_asked = Some(Instant::now());
                true
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
        }
    }

    /// Revoke `row`'s device: refused while another revoke may still change
    /// the registry, or when the device cannot be revoked from here.
    pub(crate) fn revoke(&mut self, row: &DeviceRow) -> Result<u64, Refused> {
        // The slot is held from the request until its answer was told.
        if let Some(revoke) = &self.revoke {
            return Err(if revoke.in_flight() {
                Refused::InFlight
            } else {
                Refused::Untold
            });
        }
        if !self.current() {
            return Err(Refused::NotCurrent);
        }
        if !row.actionable || row.grant == DeviceGrant::Revoked {
            return Err(Refused::NotRevocable);
        }
        let op = self.next_op + 1;
        let asked = Instant::now();
        let deadline = asked + self.admission;
        let id = row.id.clone();
        let Some(worker) = self.worker() else {
            return Err(Refused::Unavailable);
        };
        // Accepted only once the worker has it: a request it never got is
        // not a revoke that may be running.
        worker
            .requests
            .try_send(Request::Revoke { op, id, deadline })
            .map_err(|_| Refused::Unavailable)?;
        self.next_op = op;
        // From here it may commit: counts read before are not current.
        self.epoch += 1;
        self.revoke = Some(RevokeOp {
            op,
            row: row.clone(),
            state: RevokeState::Pending,
            asked,
            answered: false,
        });
        Ok(op)
    }

    /// Forget a revoke whose answer was told (a new one may then start).
    pub(crate) fn settle(&mut self, op: u64) {
        if self
            .revoke
            .as_ref()
            .is_some_and(|revoke| revoke.op == op && revoke.answered)
        {
            self.revoke = None;
        }
    }

    /// Take the worker's answers, and mark a revoke not answered in time as
    /// unknown. Returns whether anything changed.
    pub(crate) fn poll(&mut self, now: Instant) -> bool {
        let mut changed = false;
        loop {
            let reply = match self.worker.as_ref().map(|worker| worker.replies.try_recv()) {
                None | Some(Err(TryRecvError::Empty)) => break,
                Some(Err(TryRecvError::Disconnected)) => {
                    // The worker is gone: a revoke it had may or may not have
                    // been written; nothing more will be read.
                    self.worker = None;
                    // Every check it owed is settled: a retired one is
                    // dropped, a live one answers unknown, and a canvas that
                    // waited behind it gets its one check on a new worker
                    // (or answers unknown when none can start).
                    if let Some(pending) = self.status.take() {
                        if !pending.cancel.is_cancelled() {
                            self.status_ready = Some((pending.generation, None));
                        }
                    }
                    if let Some((generation, cwd)) = self.status_wanted.take() {
                        if self.check_status(generation, &cwd) != StatusAsked::Sent {
                            self.status_ready = Some((generation, None));
                        }
                    }
                    if let Some(revoke) = self.revoke.as_mut().filter(|revoke| !revoke.answered) {
                        // It may have been written before the worker died:
                        // no later change can come from it, but only a new
                        // read says what happened.
                        revoke.state = RevokeState::Unknown;
                        revoke.answered = true;
                        self.fence = self.asked_seq + 1;
                        self.mark_stale();
                    }
                    if self.read_pending {
                        self.read_pending = false;
                        self.read_failed("the device list could not be read".to_string());
                    }
                    changed = true;
                    break;
                }
                Some(Ok(reply)) => reply,
            };
            changed = true;
            match reply {
                Reply::Status { seq, status } => self.status_answered(seq, status),
                Reply::Listed { seq, rows } => {
                    if seq == self.asked_seq {
                        self.read_pending = false;
                        if std::mem::take(&mut self.reread) {
                            self.refresh();
                        }
                    }
                    if seq <= self.applied_seq || seq < self.fence {
                        continue;
                    }
                    self.applied_seq = seq;
                    match rows {
                        Ok(rows) => self.listing = Listing::Ready { rows, stale: false },
                        Err(reason) => self.read_failed(reason),
                    }
                }
                Reply::Revoked { op, result } => {
                    let Some(revoke) = self.revoke.as_mut().filter(|revoke| revoke.op == op) else {
                        continue;
                    };
                    revoke.answered = true;
                    revoke.state = match result {
                        Err(DeviceError::Busy) => RevokeState::Busy,
                        Err(error) => RevokeState::Done(Err(error.to_string())),
                        Ok(outcome) => RevokeState::Done(Ok(outcome)),
                    };
                    // What follows the answer is read afresh; earlier reads
                    // never replace it, and until then the list is stale.
                    self.fence = self.asked_seq + 1;
                    self.mark_stale();
                    if self.read_pending {
                        self.reread = true;
                    } else {
                        self.refresh();
                    }
                }
            }
        }
        if let Some(revoke) = self.revoke.as_mut() {
            if revoke.state == RevokeState::Pending
                && now.duration_since(revoke.asked) >= self.answer
            {
                revoke.state = RevokeState::Unknown;
                changed = true;
            }
        }
        changed
    }

    fn mark_stale(&mut self) {
        if let Listing::Ready { stale, .. } = &mut self.listing {
            *stale = true;
        }
    }

    fn read_failed(&mut self, reason: String) {
        self.listing = match std::mem::replace(&mut self.listing, Listing::Loading) {
            Listing::Ready { rows, .. } => Listing::Ready { rows, stale: true },
            _ => Listing::Failed(reason),
        };
    }
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::server_devices::{DeviceRegistry, RevokeOutcome};
    use localpilot_rpc::Scope;

    /// Poll until `done` holds (bounded).
    fn until(devices: &mut Devices, what: &str, done: impl Fn(&Devices) -> bool) {
        let started = Instant::now();
        loop {
            devices.poll(Instant::now());
            if done(devices) {
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "never reached {what}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn rows(devices: &Devices) -> Vec<DeviceRow> {
        match devices.listing() {
            Listing::Ready { rows, .. } => rows.rows.clone(),
            other => panic!("not listed: {other:?}"),
        }
    }

    fn registry_in(dir: &std::path::Path) -> Arc<DeviceRegistry> {
        Arc::new(DeviceRegistry::open(dir, dir))
    }

    fn audit_revocations(dir: &std::path::Path) -> usize {
        std::fs::read_to_string(dir.join("audit.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.contains("\"revoked\""))
            .count()
    }

    #[test]
    fn the_registry_says_what_a_revoke_did() {
        let registry = DeviceRegistry::default();
        let (standing, _) = registry.add("phone", Scope::Control, false).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let outcome = registry
            .revoke_device_within(&standing.id, deadline)
            .unwrap();
        assert!(
            matches!(&outcome, RevokeOutcome::Revoked(row) if row.id == standing.id && row.grant == DeviceGrant::Revoked),
            "{outcome:?}"
        );
        assert!(matches!(
            registry
                .revoke_device_within(&standing.id, deadline)
                .unwrap(),
            RevokeOutcome::AlreadyRevoked(_)
        ));
        assert_eq!(
            registry.revoke_device("nope").unwrap(),
            RevokeOutcome::NotFound
        );
        assert!(
            !registry.revoke(&standing.id).unwrap(),
            "the old answer still folds"
        );
        // The device stays refused as before.
        assert!(registry.standing(&standing.id).is_err());
        // An expired deadline changes nothing.
        let (other, _) = registry.add("tablet", Scope::Observe, true).unwrap();
        assert!(matches!(
            registry.revoke_device_within(&other.id, Instant::now()),
            Err(DeviceError::Busy)
        ));
        assert!(registry.standing(&other.id).is_ok());
        registry.fail_for_test(true);
        assert!(
            registry.revoke_device_within(&other.id, deadline).is_err(),
            "a failure is never success"
        );
    }

    #[test]
    fn rows_carry_metadata_only_and_say_how_many_there_are() {
        let registry = DeviceRegistry::default();
        let (here, _) = registry
            .add("bad\u{1b}[31mname", Scope::Control, false)
            .unwrap();
        let (_, _) = registry
            .add("only-elsewhere", Scope::Observe, true)
            .unwrap();
        registry.edit_for_test(&here.id, |record| record.name = "x".repeat(1000));
        let listed = registry.rows().unwrap();
        assert_eq!(listed.total, 2);
        assert_eq!((listed.active, listed.revoked), (2, 0));
        let row = listed.rows.iter().find(|row| row.id == here.id).unwrap();
        assert!(row.name.len() <= 256 && row.name.ends_with('…'));
        assert_eq!(row.reach, crate::server_devices::DeviceReach::All);
        assert!(row.actionable);
        let debug = format!("{listed:?}");
        assert!(!debug.contains("credential"), "{debug}");
        let elsewhere = listed
            .rows
            .iter()
            .find(|row| row.name == "only-elsewhere")
            .unwrap();
        assert!(matches!(
            elsewhere.reach,
            crate::server_devices::DeviceReach::Roots { .. }
        ));
    }

    #[test]
    fn a_held_registry_lock_makes_a_revoke_busy_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let registry = registry_in(dir.path());
        let (standing, _) = registry.add("phone", Scope::Control, false).unwrap();
        // Another holder has the write lock, as another process would.
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.path().join("registry.lock"))
            .unwrap();
        let mut lock = fd_lock::RwLock::new(lock_file);
        let held = lock.write().unwrap();
        let mut devices = Devices::new(registry.clone())
            .with_budgets(Duration::from_millis(150), Duration::from_secs(5));
        devices.refresh();
        until(&mut devices, "listed", |devices| {
            matches!(devices.listing(), Listing::Ready { .. })
        });
        let row = rows(&devices).remove(0);
        devices.revoke(&row).unwrap();
        until(&mut devices, "answered", |devices| {
            devices
                .revoke_op()
                .is_some_and(|op| op.state == RevokeState::Busy)
        });
        drop(held);
        assert!(registry.standing(&standing.id).is_ok(), "nothing changed");
        assert_eq!(audit_revocations(dir.path()), 0);
        // Busy is an answer: once told, and the list read again, a new
        // revoke may start, and works.
        let op = devices.revoke_op().unwrap().op;
        assert_eq!(
            devices.revoke(&row),
            Err(Refused::Untold),
            "never over an untold answer"
        );
        devices.settle(op);
        until(&mut devices, "current", Devices::current);
        devices.revoke(&row).unwrap();
        until(&mut devices, "revoked", |devices| {
            matches!(
                devices.revoke_op().map(|op| &op.state),
                Some(RevokeState::Done(Ok(RevokeOutcome::Revoked(_))))
            )
        });
        assert_eq!(audit_revocations(dir.path()), 1);
    }

    /// The revoke is admitted, then held past the terminal's own deadline:
    /// it stays the one revoke in flight (unknown), no other starts, and its
    /// late answer is the one result, recorded once.
    #[test]
    fn a_revoke_held_after_admission_stays_the_one_in_flight_until_it_answers() {
        let dir = tempfile::tempdir().unwrap();
        let registry = registry_in(dir.path());
        let (standing, _) = registry.add("phone", Scope::Control, false).unwrap();
        let (second, _) = registry.add("tablet", Scope::Control, false).unwrap();
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let gate = std::sync::Mutex::new(gate);
        registry.on_admitted_for_test(Some(Arc::new(move || {
            let _ = gate.lock().unwrap().recv();
        })));
        let mut devices = Devices::new(registry.clone())
            .with_budgets(Duration::from_secs(5), Duration::from_millis(100));
        devices.refresh();
        until(&mut devices, "listed", |devices| {
            matches!(devices.listing(), Listing::Ready { .. })
        });
        let listed = rows(&devices);
        let target = listed
            .iter()
            .find(|row| row.id == standing.id)
            .unwrap()
            .clone();
        let other = listed
            .iter()
            .find(|row| row.id == second.id)
            .unwrap()
            .clone();
        devices.revoke(&target).unwrap();
        until(&mut devices, "unknown", |devices| {
            devices
                .revoke_op()
                .is_some_and(|op| op.state == RevokeState::Unknown)
        });
        // Retry, another device, and Refresh: no second mutation starts.
        assert_eq!(devices.revoke(&target), Err(Refused::InFlight));
        assert_eq!(devices.revoke(&other), Err(Refused::InFlight));
        assert!(devices.refresh());
        assert!(
            devices.refresh(),
            "a repeat while one is pending does nothing"
        );
        devices.poll(Instant::now());
        assert_eq!(devices.revoke_op().unwrap().state, RevokeState::Unknown);
        // The late commit.
        release.send(()).unwrap();
        until(&mut devices, "answered", |devices| {
            devices
                .revoke_op()
                .is_some_and(|op| matches!(op.state, RevokeState::Done(_)))
        });
        assert!(matches!(
            &devices.revoke_op().unwrap().state,
            RevokeState::Done(Ok(RevokeOutcome::Revoked(row))) if row.id == standing.id
        ));
        until(&mut devices, "re-read", |devices| !devices.reading());
        let after = rows(&devices);
        assert_eq!(
            after
                .iter()
                .find(|row| row.id == standing.id)
                .unwrap()
                .grant,
            DeviceGrant::Revoked,
            "the read after the answer shows it; the one queued before never replaces it"
        );
        assert_eq!(
            audit_revocations(dir.path()),
            1,
            "one mutation, recorded once"
        );
        assert!(registry.standing(&second.id).is_ok());
    }

    #[test]
    fn a_worker_lost_mid_revoke_leaves_its_result_unknown() {
        let registry = Arc::new(DeviceRegistry::default());
        let (standing, _) = registry.add("phone", Scope::Control, false).unwrap();
        let mut devices = Devices::new(registry.clone());
        devices.refresh();
        until(&mut devices, "listed", |devices| {
            matches!(devices.listing(), Listing::Ready { .. })
        });
        let row = rows(&devices).remove(0);
        registry.on_admitted_for_test(Some(Arc::new(|| panic!("the worker dies here"))));
        devices.revoke(&row).unwrap();
        until(&mut devices, "unknown", |devices| {
            devices
                .revoke_op()
                .is_some_and(|op| op.state == RevokeState::Unknown && !op.in_flight())
        });
        let _ = standing;
    }

    #[test]
    fn dropping_the_terminal_side_never_waits_for_an_admitted_revoke() {
        let dir = tempfile::tempdir().unwrap();
        let registry = registry_in(dir.path());
        let (standing, _) = registry.add("phone", Scope::Control, false).unwrap();
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let gate = std::sync::Mutex::new(gate);
        let (admitted_tx, admitted) = std::sync::mpsc::channel::<()>();
        let admitted_tx = std::sync::Mutex::new(admitted_tx);
        registry.on_admitted_for_test(Some(Arc::new(move || {
            let _ = admitted_tx.lock().unwrap().send(());
            let _ = gate.lock().unwrap().recv();
        })));
        let mut devices = Devices::new(registry.clone());
        devices.refresh();
        until(&mut devices, "listed", |devices| {
            matches!(devices.listing(), Listing::Ready { .. })
        });
        let row = rows(&devices).remove(0);
        devices.revoke(&row).unwrap();
        admitted.recv_timeout(Duration::from_secs(5)).unwrap();
        let dropped = Instant::now();
        drop(devices);
        assert!(
            dropped.elapsed() < Duration::from_millis(500),
            "never joined"
        );
        // The admitted revoke completes while the process lives.
        release.send(()).unwrap();
        let started = Instant::now();
        while registry.standing(&standing.id).is_ok() {
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_failed_reread_keeps_the_last_rows_marked_stale() {
        let registry = Arc::new(DeviceRegistry::default());
        registry.add("phone", Scope::Control, false).unwrap();
        let mut devices = Devices::new(registry.clone());
        devices.refresh();
        until(&mut devices, "listed", |devices| {
            matches!(devices.listing(), Listing::Ready { .. })
        });
        registry.fail_for_test(true);
        devices.refresh();
        until(&mut devices, "stale", |devices| {
            matches!(devices.listing(), Listing::Ready { stale: true, .. })
        });
        let fresh = Devices::new(registry.clone());
        let mut fresh = fresh;
        fresh.refresh();
        until(&mut fresh, "failed", |devices| {
            matches!(devices.listing(), Listing::Failed(_))
        });
    }
    /// The lock frees only after the deadline: the attempt that then gets it
    /// lets it go untouched.
    #[test]
    fn a_lock_won_after_the_deadline_is_let_go_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let registry = registry_in(dir.path());
        let (standing, _) = registry.add("phone", Scope::Control, false).unwrap();
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.path().join("registry.lock"))
            .unwrap();
        let holder = std::sync::Mutex::new(Some(fd_lock::RwLock::new(lock_file)));
        // Hold it now; the first attempt releases it only after the deadline.
        let deadline = Instant::now() + Duration::from_millis(100);
        let mut guard_holder = holder.lock().unwrap();
        let lock = guard_holder.as_mut().unwrap();
        let held = lock.write().unwrap();
        std::mem::forget(held);
        drop(guard_holder);
        let holder = Arc::new(holder);
        let admitted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        registry.hook_at_for_test(
            crate::server_devices::Hook::Attempting,
            Some({
                let holder = holder.clone();
                Arc::new(move || {
                    if let Some(lock) = holder.lock().unwrap().take() {
                        // Past the deadline, then free: the try succeeds.
                        while Instant::now() < deadline + Duration::from_millis(20) {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        drop(lock);
                    }
                })
            }),
        );
        registry.on_admitted_for_test(Some({
            let admitted = admitted.clone();
            Arc::new(move || admitted.store(true, std::sync::atomic::Ordering::SeqCst))
        }));
        // The first check passes (before the deadline); the attempt then
        // wins the lock after it.
        let result = registry.revoke_device_within(&standing.id, deadline);
        assert!(matches!(result, Err(DeviceError::Busy)), "{result:?}");
        assert!(
            !admitted.load(std::sync::atomic::Ordering::SeqCst),
            "never admitted"
        );
        assert!(registry.standing(&standing.id).is_ok(), "nothing changed");
        assert_eq!(audit_revocations(dir.path()), 0);
    }

    #[test]
    fn totals_count_the_whole_registry_not_the_rows_shown() {
        let registry = DeviceRegistry::default();
        for n in 0..300 {
            let (standing, _) = registry
                .add(&format!("d{n}"), Scope::Observe, false)
                .unwrap();
            if n % 3 == 0 {
                registry.revoke(&standing.id).unwrap();
            }
        }
        let listed = registry.rows().unwrap();
        assert_eq!(listed.rows.len(), crate::server_devices::ROWS_SHOWN);
        assert_eq!(listed.total, 300);
        assert_eq!((listed.active, listed.revoked), (200, 100));
    }

    #[test]
    fn a_row_whose_record_names_another_device_cannot_be_revoked() {
        let registry = Arc::new(DeviceRegistry::default());
        let (first, _) = registry.add("first", Scope::Control, false).unwrap();
        let (second, _) = registry.add("second", Scope::Control, false).unwrap();
        // The first record claims the second's id.
        registry.edit_for_test(&first.id, |record| record.id = second.id.clone());
        let listed = registry.rows().unwrap();
        let corrupted = listed.rows.iter().find(|row| row.name == "first").unwrap();
        assert_eq!(corrupted.id, first.id, "shown under its own key");
        assert!(!corrupted.actionable);
        let mut devices = Devices::new(registry.clone());
        devices.refresh();
        until(&mut devices, "listed", Devices::current);
        let row = rows(&devices)
            .into_iter()
            .find(|row| row.name == "first")
            .unwrap();
        assert_eq!(devices.revoke(&row), Err(Refused::NotRevocable));
        assert!(
            registry.standing(&second.id).is_ok(),
            "the other device stays active"
        );
        // A long or control-bearing key never becomes actionable either.
        let (third, _) = registry.add("third", Scope::Control, false).unwrap();
        registry.edit_for_test(&third.id, |record| {
            record.id = format!("{}\u{1b}x", third.id)
        });
        let listed = registry.rows().unwrap();
        assert!(
            !listed
                .rows
                .iter()
                .find(|row| row.name == "third")
                .unwrap()
                .actionable
        );
    }

    #[test]
    fn an_answer_holds_the_slot_until_told_and_the_list_is_read_again() {
        let registry = Arc::new(DeviceRegistry::default());
        registry.add("a", Scope::Control, false).unwrap();
        registry.add("b", Scope::Control, false).unwrap();
        let mut devices = Devices::new(registry.clone());
        devices.refresh();
        until(&mut devices, "listed", Devices::current);
        let listed = rows(&devices);
        let op = devices.revoke(&listed[0]).unwrap();
        until(&mut devices, "answered", |devices| {
            devices
                .revoke_op()
                .is_some_and(|op| matches!(op.state, RevokeState::Done(_)))
        });
        // Answered, not yet told: nothing replaces it.
        assert_eq!(devices.revoke(&listed[1]), Err(Refused::Untold));
        assert!(matches!(
            devices.revoke_op().unwrap().state,
            RevokeState::Done(Ok(_))
        ));
        devices.settle(op);
        // The list read before the answer is stale until the read after it.
        if !devices.current() {
            assert_eq!(devices.revoke(&listed[1]), Err(Refused::NotCurrent));
        }
        until(&mut devices, "current", Devices::current);
        let fresh = rows(&devices)
            .into_iter()
            .find(|row| row.id == listed[1].id)
            .unwrap();
        devices.revoke(&fresh).unwrap();
    }

    /// The worker dies after writing the revoke, before answering: the
    /// result is unknown, and no new revoke starts until a fresh read shows
    /// what happened.
    #[test]
    fn a_worker_lost_after_writing_needs_a_fresh_read_before_another_revoke() {
        let dir = tempfile::tempdir().unwrap();
        let registry = registry_in(dir.path());
        let (standing, _) = registry.add("phone", Scope::Control, false).unwrap();
        registry.add("tablet", Scope::Control, false).unwrap();
        let mut devices = Devices::new(registry.clone());
        devices.refresh();
        until(&mut devices, "listed", Devices::current);
        let listed = rows(&devices);
        let target = listed
            .iter()
            .find(|row| row.id == standing.id)
            .unwrap()
            .clone();
        let other = listed
            .iter()
            .find(|row| row.id != standing.id)
            .unwrap()
            .clone();
        registry.hook_at_for_test(
            crate::server_devices::Hook::Committed,
            Some(Arc::new(|| panic!("the worker dies after the write"))),
        );
        let op = devices.revoke(&target).unwrap();
        until(&mut devices, "unknown", |devices| {
            devices
                .revoke_op()
                .is_some_and(|op| op.state == RevokeState::Unknown && !op.in_flight())
        });
        registry.hook_at_for_test(crate::server_devices::Hook::Committed, None);
        assert!(registry.standing(&standing.id).is_err(), "it was written");
        assert_eq!(devices.revoke(&other), Err(Refused::Untold));
        devices.settle(op);
        assert!(!devices.current());
        assert_eq!(devices.revoke(&other), Err(Refused::NotCurrent));
        // A fresh read (a new worker) reconciles it.
        devices.refresh();
        until(&mut devices, "current", Devices::current);
        assert_eq!(
            rows(&devices)
                .iter()
                .find(|row| row.id == standing.id)
                .unwrap()
                .grant,
            DeviceGrant::Revoked
        );
        let other = rows(&devices)
            .into_iter()
            .find(|row| row.id == other.id)
            .unwrap();
        devices.revoke(&other).unwrap();
    }

    #[test]
    fn a_slow_read_is_said_to_be_late_and_never_doubled() {
        let registry = Arc::new(DeviceRegistry::default());
        let mut devices = Devices::new(registry);
        assert!(devices.refresh());
        assert!(devices.refresh());
        assert!(!devices.reading_late(Instant::now()));
        assert!(devices.reading_late(Instant::now() + READ_ANSWER));
        until(&mut devices, "listed", Devices::current);
    }
    /// A status check the test controls: it waits for a release, then
    /// answers (or sees its cancel).
    fn gated_check() -> (
        StatusCheck,
        std::sync::mpsc::Sender<()>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let gate = std::sync::Mutex::new(gate);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let check: StatusCheck = {
            let calls = calls.clone();
            Arc::new(move |_cwd: &std::path::Path, cancel: &CancellationToken| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = gate.lock().unwrap().recv();
                if cancel.is_cancelled() {
                    return None;
                }
                let now = std::time::SystemTime::now();
                Some(crate::remote_status::ObservedStatus {
                    status: crate::remote_status::RemoteStatus {
                        settings: Err("test".into()),
                        gateway: None,
                        workspaces: Vec::new(),
                        devices: Ok((1, 0)),
                    },
                    settings_at: now,
                    gateway_at: now,
                    devices_at: now,
                })
            })
        };
        (check, release, calls)
    }

    #[test]
    fn one_status_check_at_a_time_and_a_stale_answer_never_reaches_a_new_canvas() {
        let (check, release, calls) = gated_check();
        let mut devices = Devices::new(Arc::new(DeviceRegistry::default())).with_check(check);
        let cwd = std::path::Path::new(".");
        assert_eq!(devices.check_status(1, cwd), StatusAsked::Sent);
        assert_eq!(
            devices.check_status(1, cwd),
            StatusAsked::Coalesced,
            "Refresh coalesces"
        );
        // Canvas 1 closes while its check is blocked; canvas 2 opens.
        while calls.load(std::sync::atomic::Ordering::SeqCst) < 1 {
            std::thread::sleep(Duration::from_millis(5));
        }
        devices.retire_status(1);
        assert_eq!(devices.check_status(2, cwd), StatusAsked::Queued);
        assert_eq!(devices.check_status(2, cwd), StatusAsked::Queued);
        assert!(devices.checking_status(2));
        // The old check answers: dropped for canvas 2, which gets exactly one.
        release.send(()).unwrap();
        let started = Instant::now();
        while calls.load(std::sync::atomic::Ordering::SeqCst) < 2 {
            devices.poll(Instant::now());
            assert!(
                devices.take_status(2).is_none(),
                "the stale answer never reaches it"
            );
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
        release.send(()).unwrap();
        let probe = loop {
            devices.poll(Instant::now());
            if let Some(probe) = devices.take_status(2) {
                break probe;
            }
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(probe.is_some(), "its own check answered");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "no accumulation"
        );
        assert!(!devices.checking_status(2));
    }

    /// A check cancelled before the worker starts it skips the network part.
    #[test]
    fn a_check_cancelled_while_queued_never_runs() {
        let (check, release, calls) = gated_check();
        let mut devices = Devices::new(Arc::new(DeviceRegistry::default())).with_check(check);
        devices.check_status(1, std::path::Path::new("."));
        devices.retire_status(1);
        drop(release);
        let started = Instant::now();
        while devices.status.is_some() {
            devices.poll(Instant::now());
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(calls.load(std::sync::atomic::Ordering::SeqCst) <= 1);
        assert!(devices.take_status(1).is_none());
    }

    #[test]
    fn a_cancelled_check_answers_cancelled_and_frees_the_slot() {
        let (check, release, calls) = gated_check();
        let mut devices = Devices::new(Arc::new(DeviceRegistry::default())).with_check(check);
        let cwd = std::path::Path::new(".");
        devices.check_status(1, cwd);
        while calls.load(std::sync::atomic::Ordering::SeqCst) < 1 {
            std::thread::sleep(Duration::from_millis(5));
        }
        // Cancelled during the exchange (the check sees its token).
        devices.retire_status(1);
        release.send(()).unwrap();
        let started = Instant::now();
        while devices.status.is_some() {
            devices.poll(Instant::now());
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            devices.take_status(1).is_none(),
            "a retired canvas gets nothing"
        );
        // A new canvas can check right away.
        assert_eq!(devices.check_status(3, cwd), StatusAsked::Sent);
        assert!(devices.status_late(Instant::now() + STATUS_ANSWER));
        drop(release);
    }

    #[test]
    fn a_revoke_accepted_during_a_check_keeps_its_counts_from_being_current() {
        let (check, release, _) = gated_check();
        let registry = Arc::new(DeviceRegistry::default());
        registry.add("phone", Scope::Control, false).unwrap();
        let mut devices = Devices::new(registry).with_check(check);
        devices.refresh();
        until(&mut devices, "listed", Devices::current);
        let cwd = std::path::Path::new(".");
        devices.check_status(1, cwd);
        let row = rows(&devices).remove(0);
        // Accepted while the check runs: it may commit.
        devices.revoke(&row).unwrap();
        assert_eq!(devices.epoch(), 1);
        release.send(()).unwrap();
        let started = Instant::now();
        let probe = loop {
            devices.poll(Instant::now());
            if let Some(probe) = devices.take_status(1) {
                break probe.unwrap();
            }
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(probe.epoch, 0, "asked before the change");
        let mut model = crate::remote_status_model::StatusModel::default();
        model.merge(probe, devices.epoch());
        assert!(
            model.counts_line().starts_with("Last known"),
            "{}",
            model.counts_line()
        );
    }

    #[test]
    fn a_worker_lost_during_a_check_answers_it_as_unknown() {
        let registry = Arc::new(DeviceRegistry::default());
        let check: StatusCheck =
            Arc::new(|_: &std::path::Path, _: &CancellationToken| panic!("the worker dies"));
        let mut devices = Devices::new(registry).with_check(check);
        devices.check_status(1, std::path::Path::new("."));
        let started = Instant::now();
        let answer = loop {
            devices.poll(Instant::now());
            if let Some(answer) = devices.take_status(1) {
                break answer;
            }
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(answer.is_none());
        assert!(!devices.checking_status(1));
    }
    /// A retired canvas's check is blocked, a new canvas waits behind it,
    /// and the worker dies: the new canvas gets its one check on a new
    /// worker, never perpetual checking, and never the old answer.
    #[test]
    fn a_canvas_waiting_behind_a_dying_check_still_gets_its_answer() {
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let gate = std::sync::Mutex::new(gate);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let check: StatusCheck = {
            let calls = calls.clone();
            Arc::new(move |_: &std::path::Path, _: &CancellationToken| {
                let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = gate.lock().unwrap().recv();
                assert!(call > 0, "the first check dies");
                let now = std::time::SystemTime::now();
                Some(crate::remote_status::ObservedStatus {
                    status: crate::remote_status::RemoteStatus {
                        settings: Err("test".into()),
                        gateway: None,
                        workspaces: Vec::new(),
                        devices: Ok((7, 0)),
                    },
                    settings_at: now,
                    gateway_at: now,
                    devices_at: now,
                })
            })
        };
        let mut devices = Devices::new(Arc::new(DeviceRegistry::default())).with_check(check);
        let cwd = std::path::Path::new(".");
        devices.check_status(1, cwd);
        while calls.load(std::sync::atomic::Ordering::SeqCst) < 1 {
            std::thread::sleep(Duration::from_millis(5));
        }
        devices.retire_status(1);
        assert_eq!(devices.check_status(2, cwd), StatusAsked::Queued);
        release.send(()).unwrap(); // the first check panics: the worker dies
        let started = Instant::now();
        while calls.load(std::sync::atomic::Ordering::SeqCst) < 2 {
            devices.poll(Instant::now());
            assert!(devices.take_status(1).is_none());
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "never perpetual checking"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        release.send(()).unwrap();
        let answer = loop {
            devices.poll(Instant::now());
            if let Some(answer) = devices.take_status(2) {
                break answer;
            }
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(
            answer.map(|probe| probe.observed.status.devices),
            Some(Ok((7, 0))),
            "its own check, on a new worker"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(!devices.checking_status(2));
    }
}
