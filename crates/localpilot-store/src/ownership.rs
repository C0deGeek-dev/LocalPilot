//! Exclusive session ownership between cooperating LocalPilot processes.
//!
//! A session is run by at most one runtime at a time. The runtime that owns a
//! session holds an exclusive operating-system lock on
//! `sessions/<id>.owner.lock` for as long as it runs it; the operating system
//! drops the lock when the process ends, so a free lock proves that no live
//! process owns the session, without any timeout or heartbeat.
//!
//! Next to the lock, `sessions/<id>.owner.json` records the ownership
//! generation (one more on every claim), the holder, and the state of a
//! hand-over to another process. It is only ever published while the lock is
//! held.
//!
//! A [`SessionClaim`] also arms a lease in the lease table of the
//! [`Store`](crate::Store) handle that took it (shared by that handle's clones,
//! not by other handles opened on the same directory). Every session-owned write
//! through that handle checks the lease under the same mutex as the write
//! itself. Releasing a claim for a hand-over kills the lease before the lock is
//! let go, and the dead lease stays, so the handle and its clones can never
//! write to the session again until they claim it anew. A handle with no lease
//! for a session writes only while it can take the session's lock itself, so it
//! can never write to a session another owner holds, whatever spelling of the
//! directory it was opened with.
//!
//! Lock files are never deleted or replaced, not even by prune while it holds
//! one: an opener that already opened the file could otherwise lock an orphaned
//! file while another opener locks a new one under the same name.
//!
//! Older builds ignore these files; ownership is enforced only between builds
//! that take the lock.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use localpilot_core::{EventId, SessionId};
use serde::{Deserialize, Serialize};

use crate::atomic::atomic_write;
use crate::StoreError;

/// The owner-record format version this build writes and reads.
pub const OWNER_RECORD_VERSION: u32 = 1;

/// What kind of process holds a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum HolderKind {
    /// An interactive terminal session.
    Tui,
    /// The workspace's background server (`localpilot serve`).
    Server,
    /// Any other runtime (headless runs, maintenance such as prune).
    Other,
}

/// Who holds (or last held) a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    /// A random identifier of the holding runtime instance.
    pub instance: String,
    /// The holding process id, for messages only (never used to decide).
    pub pid: u32,
    pub kind: HolderKind,
}

impl Holder {
    /// A fresh holder identity for this process.
    #[must_use]
    pub fn this_process(kind: HolderKind) -> Self {
        Self {
            instance: SessionId::new().to_string(),
            pid: std::process::id(),
            kind,
        }
    }
}

/// How far a hand-over of the session to another process got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferState {
    /// The source let the session go for this transfer; nobody drives it until
    /// the destination commits.
    Released,
    /// The destination took the session over.
    Committed,
}

/// A hand-over recorded in the owner record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferRecord {
    /// The transfer's identifier, chosen by the source.
    pub id: String,
    pub state: TransferState,
    /// The last event of the session log when the source froze it; `None` for a
    /// session with no events yet.
    pub tail: Option<EventId>,
    /// Digest of the transferred settings, so both sides can prove they agree.
    pub manifest_digest: String,
}

/// The published owner record of a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerRecord {
    pub v: u32,
    /// One more on every claim; a released or committed record keeps the
    /// generation of the claim that wrote it.
    pub generation: u64,
    pub holder: Holder,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer: Option<TransferRecord>,
}

/// The lease that fences one session's writes inside this process.
#[derive(Debug)]
pub(crate) struct Lease {
    live: Mutex<bool>,
}

impl Lease {
    /// Run `write` only while the lease is live, holding the lease's mutex for
    /// the whole write, so a release can never interleave with it.
    pub(crate) fn guarded<T>(
        &self,
        session: SessionId,
        write: impl FnOnce() -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let live = lock_ignoring_poison(&self.live);
        if !*live {
            return Err(StoreError::OwnershipLost { session });
        }
        write()
    }
}

/// The leases one store handle (and its clones) armed, by session. A released
/// lease stays here (dead); a claim dropped without a hand-over removes its
/// entry, so later writes need the session's lock again.
pub(crate) type LeaseTable = Arc<Mutex<HashMap<SessionId, Arc<Lease>>>>;

pub(crate) fn lease_in(table: &LeaseTable, session: SessionId) -> Option<Arc<Lease>> {
    lock_ignoring_poison(table).get(&session).cloned()
}

fn lock_ignoring_poison<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// An exclusive OS lock held by a dedicated thread for as long as this value
/// lives. The thread exists so the lock guard can be owned without unsafe
/// self-references; it does nothing but wait.
#[derive(Debug)]
pub(crate) struct HeldLock {
    release: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl HeldLock {
    /// Try to take the lock at `path` once. `Ok(None)` means another holder has
    /// it.
    fn try_take(path: &Path) -> io::Result<Option<Self>> {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        let (ready_tx, ready_rx) = mpsc::channel::<io::Result<bool>>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("localpilot-session-owner".to_string())
            .spawn(move || {
                let mut lock = fd_lock::RwLock::new(file);
                match lock.try_write() {
                    Ok(guard) => {
                        let _ = ready_tx.send(Ok(true));
                        // Returns when the sender is dropped: release.
                        let _ = release_rx.recv();
                        drop(guard);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        let _ = ready_tx.send(Ok(false));
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                    }
                };
            })?;
        let outcome = ready_rx
            .recv()
            .unwrap_or_else(|_| Err(io::Error::other("the session lock thread ended early")));
        match outcome {
            Ok(true) => Ok(Some(Self {
                release: Some(release_tx),
                thread: Some(thread),
            })),
            Ok(false) => {
                let _ = thread.join();
                Ok(None)
            }
            Err(error) => {
                let _ = thread.join();
                Err(error)
            }
        }
    }
}

impl Drop for HeldLock {
    fn drop(&mut self) {
        drop(self.release.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Exclusive ownership of one session by this process.
///
/// Dropping the claim lets the session go without a hand-over: the lease is
/// killed (after any write running under it finishes), its table entry removed,
/// and only then the lock released. [`SessionClaim::release`] is
/// the hand-over path: it kills the lease first, so nothing in this process can
/// write to the session afterwards.
#[derive(Debug)]
pub struct SessionClaim {
    session: SessionId,
    table: LeaseTable,
    record_path: PathBuf,
    lease: Arc<Lease>,
    previous: Option<OwnerRecord>,
    record: OwnerRecord,
    released: bool,
    // Declared last so it is dropped after the fields above.
    lock: Option<HeldLock>,
}

impl SessionClaim {
    pub(crate) fn take(
        table: &LeaseTable,
        sessions_dir: &Path,
        session: SessionId,
        holder: Holder,
        wait: Duration,
    ) -> Result<Self, StoreError> {
        fs::create_dir_all(sessions_dir).map_err(|error| StoreError::io(sessions_dir, error))?;
        let lock_path = lock_path(sessions_dir, session);
        let record_path = record_path(sessions_dir, session);
        let deadline = Instant::now() + wait;
        let lock = loop {
            match HeldLock::try_take(&lock_path) {
                Ok(Some(lock)) => break lock,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => {
                    return Err(StoreError::SessionBusy {
                        session,
                        holder: read_record(&record_path)
                            .ok()
                            .flatten()
                            .map(|record| record.holder),
                    });
                }
                Err(error) => return Err(StoreError::io(&lock_path, error)),
            }
        };
        let previous = read_record(&record_path)?;
        let generation = previous
            .as_ref()
            .map_or(0, |record| record.generation)
            .saturating_add(1);
        let record = OwnerRecord {
            v: OWNER_RECORD_VERSION,
            generation,
            holder,
            transfer: None,
        };
        let lease = Arc::new(Lease {
            live: Mutex::new(true),
        });
        // Publish the new generation before anything can write as this owner.
        publish(&record_path, &record)?;
        lock_ignoring_poison(table).insert(session, Arc::clone(&lease));
        Ok(Self {
            session,
            table: Arc::clone(table),
            record_path,
            lease,
            previous,
            record,
            released: false,
            lock: Some(lock),
        })
    }

    /// The session this claim owns.
    #[must_use]
    pub fn session(&self) -> SessionId {
        self.session
    }

    /// This claim's ownership generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.record.generation
    }

    /// The owner record found when the claim was taken (`None` for a session
    /// never claimed before). A destination reads the source's released transfer
    /// here.
    #[must_use]
    pub fn previous(&self) -> Option<&OwnerRecord> {
        self.previous.as_ref()
    }

    /// The record this claim published last.
    #[must_use]
    pub fn record(&self) -> &OwnerRecord {
        &self.record
    }

    /// Publish `transfer` as committed by this claim: the destination's side of
    /// a hand-over. The record is durable when this returns, before the caller
    /// drives the session.
    ///
    /// # Errors
    /// Returns the store error if the record cannot be published; the previous
    /// record is then unchanged.
    pub fn commit_transfer(&mut self, transfer: TransferRecord) -> Result<(), StoreError> {
        let record = OwnerRecord {
            transfer: Some(TransferRecord {
                state: TransferState::Committed,
                ..transfer
            }),
            ..self.record.clone()
        };
        publish(&self.record_path, &record)?;
        self.record = record;
        Ok(())
    }

    /// Let the session go for a hand-over. Under the lease mutex the lease dies
    /// and the released record is published while the lock is still held; only
    /// when that succeeded is the lock dropped. On failure the lease is live
    /// again, the lock is still held and the claim is returned, so the caller is
    /// still the sole owner.
    ///
    /// # Errors
    /// Returns the claim and the store error when the record cannot be
    /// published.
    pub fn release(mut self, transfer: TransferRecord) -> Result<(), (Self, StoreError)> {
        let record = OwnerRecord {
            transfer: Some(TransferRecord {
                state: TransferState::Released,
                ..transfer
            }),
            ..self.record.clone()
        };
        {
            let mut live = lock_ignoring_poison(&self.lease.live);
            *live = false;
            if let Err(error) = publish(&self.record_path, &record) {
                *live = true;
                drop(live);
                return Err((self, error));
            }
        }
        self.record = record;
        self.released = true;
        // The dead lease stays registered so stale handles keep failing.
        drop(self.lock.take());
        Ok(())
    }
}

impl Drop for SessionClaim {
    fn drop(&mut self) {
        if !self.released {
            // Kill the lease first: taking its mutex waits for a write already
            // running under it, and every handle still holding this lease is
            // refused from now on, before the lock is let go.
            *lock_ignoring_poison(&self.lease.live) = false;
            let mut leases = lock_ignoring_poison(&self.table);
            if leases
                .get(&self.session)
                .is_some_and(|lease| Arc::ptr_eq(lease, &self.lease))
            {
                leases.remove(&self.session);
            }
        }
        drop(self.lock.take());
    }
}

pub(crate) fn lock_path(sessions_dir: &Path, session: SessionId) -> PathBuf {
    sessions_dir.join(format!("{session}.owner.lock"))
}

pub(crate) fn record_path(sessions_dir: &Path, session: SessionId) -> PathBuf {
    sessions_dir.join(format!("{session}.owner.json"))
}

/// Read a session's owner record. A record of an unknown version is an error,
/// never a guess.
pub(crate) fn read_record(path: &Path) -> Result<Option<OwnerRecord>, StoreError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(StoreError::io(path, error)),
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    let version = value.get("v").and_then(serde_json::Value::as_u64);
    if version != Some(u64::from(OWNER_RECORD_VERSION)) {
        return Err(StoreError::UnsupportedFormat {
            found: version.unwrap_or(0),
            supported: OWNER_RECORD_VERSION,
        });
    }
    Ok(Some(serde_json::from_value(value)?))
}

fn publish(path: &Path, record: &OwnerRecord) -> Result<(), StoreError> {
    atomic_write(path, serde_json::to_string_pretty(record)?.as_bytes())
}

/// Run `work` under the workspace index's stable lock. The lock is the innermost
/// of the store's locks and is held only for one index transaction.
pub(crate) fn with_index_lock<T>(
    sessions_dir: &Path,
    work: impl FnOnce() -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    fs::create_dir_all(sessions_dir).map_err(|error| StoreError::io(sessions_dir, error))?;
    let path = sessions_dir.join("index.lock");
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| StoreError::io(&path, error))?;
    let mut lock = fd_lock::RwLock::new(file);
    let _held = lock.write().map_err(|error| StoreError::io(&path, error))?;
    work()
}

/// Run `write` for a session this handle holds no lease for, under the
/// session's lock for the duration of the write. Fails with
/// [`StoreError::SessionBusy`] when another owner holds the session, so a handle
/// that never claimed a session can never write to one somebody runs.
pub(crate) fn with_session_lock<T>(
    sessions_dir: &Path,
    session: SessionId,
    write: impl FnOnce() -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    fs::create_dir_all(sessions_dir).map_err(|error| StoreError::io(sessions_dir, error))?;
    let path = lock_path(sessions_dir, session);
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|error| StoreError::io(&path, error))?;
    let mut lock = fd_lock::RwLock::new(file);
    let _held = match lock.try_write() {
        Ok(guard) => guard,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            return Err(StoreError::SessionBusy {
                session,
                holder: read_record(&record_path(sessions_dir, session))
                    .ok()
                    .flatten()
                    .map(|record| record.holder),
            });
        }
        Err(error) => return Err(StoreError::io(&path, error)),
    };
    write()
}

/// Hold a session's lock without publishing anything (`None` when another
/// owner holds it). Maintenance such as prune uses this, so planning or
/// removing never changes an owner record it does not delete.
pub(crate) fn hold_session(
    sessions_dir: &Path,
    session: SessionId,
) -> Result<Option<HeldLock>, StoreError> {
    fs::create_dir_all(sessions_dir).map_err(|error| StoreError::io(sessions_dir, error))?;
    let path = lock_path(sessions_dir, session);
    HeldLock::try_take(&path).map_err(|error| StoreError::io(&path, error))
}

#[cfg(test)]
#[path = "ownership_tests.rs"]
mod tests;
