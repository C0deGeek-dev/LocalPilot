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
/// Requests and answers waiting at once (the terminal keeps at most one read
/// and one revoke outstanding).
const QUEUE: usize = 4;

enum Request {
    List {
        seq: u64,
    },
    Revoke {
        op: u64,
        id: String,
        deadline: Instant,
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
}

/// The one thread that does registry work, in order.
struct Worker {
    requests: SyncSender<Request>,
    replies: Receiver<Reply>,
}

impl Worker {
    fn start(registry: Arc<DeviceRegistry>) -> std::io::Result<Self> {
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
            self.worker = Worker::start(self.registry.clone()).ok();
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
}
