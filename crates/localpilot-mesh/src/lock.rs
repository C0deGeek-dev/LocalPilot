//! Lock files (spec L-5, L-6).
//!
//! A lock is a file created exclusively and released by deleting it. This is
//! the same protocol every implementation uses on the same mailbox, so it is
//! not an OS lock: two implementations must see each other's locks.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::error::MeshError;
use crate::timefmt::utc_now;

/// How often a busy lock is retried.
pub const RETRY: Duration = Duration::from_millis(50);
/// How long acquisition waits before it gives up.
pub const DEADLINE: Duration = Duration::from_secs(10);
/// A lock file older than this may be reaped once, as a dead holder's.
pub const STALE: Duration = Duration::from_secs(60);

/// A held lock; dropping it releases the lock.
#[derive(Debug)]
pub struct Lock {
    path: PathBuf,
}

impl Lock {
    /// Acquire the lock at `path` within [`DEADLINE`].
    ///
    /// # Errors
    /// [`MeshError::LockBusy`] if it is still held at the deadline (after one
    /// reap of a stale lock); [`MeshError::Io`] for other failures.
    pub fn acquire(path: &Path) -> Result<Self, MeshError> {
        Self::acquire_within(path, DEADLINE, STALE)
    }

    /// [`Lock::acquire`] with explicit timings (for tests).
    ///
    /// # Errors
    /// As [`Lock::acquire`].
    pub fn acquire_within(
        path: &Path,
        deadline: Duration,
        stale: Duration,
    ) -> Result<Self, MeshError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| MeshError::io(parent, e))?;
        }
        let end = Instant::now() + deadline;
        let mut reaped = false;
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(mut f) => {
                    // Informational only; a failure to write it does not
                    // matter, the file's existence is the lock.
                    let _ = writeln!(f, "{} {}", std::process::id(), utc_now());
                    return Ok(Self {
                        path: path.to_path_buf(),
                    });
                }
                // On Windows, re-creating a lock file whose delete is still
                // pending is refused as access denied: the same "busy".
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::AlreadyExists | io::ErrorKind::PermissionDenied
                    ) =>
                {
                    if Instant::now() < end {
                        std::thread::sleep(RETRY);
                        continue;
                    }
                    match fs::metadata(path).and_then(|m| m.modified()) {
                        Ok(modified) if !reaped && age(modified) > stale => {
                            let _ = fs::remove_file(path);
                            reaped = true;
                        }
                        // Released right at the deadline: one short grace.
                        Err(ref m)
                            if m.kind() == io::ErrorKind::NotFound
                                && Instant::now() < end + Duration::from_secs(1) =>
                        {
                            std::thread::sleep(RETRY);
                        }
                        _ => return Err(MeshError::LockBusy(path.display().to_string())),
                    }
                }
                Err(e) => return Err(MeshError::io(path, e)),
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn age(modified: SystemTime) -> Duration {
    SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_is_a_file_that_exists_while_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal").join("codex.lock");
        {
            let _held = Lock::acquire(&path).unwrap();
            assert!(path.exists());
            let body = fs::read_to_string(&path).unwrap();
            assert!(body.starts_with(&format!("{} ", std::process::id())));
        }
        assert!(!path.exists(), "released on drop");
    }

    #[test]
    fn a_held_lock_fails_bounded_and_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let _held = Lock::acquire(&path).unwrap();
        let started = Instant::now();
        let err = Lock::acquire_within(&path, Duration::from_millis(300), STALE).unwrap_err();
        assert!(matches!(err, MeshError::LockBusy(_)));
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(path.exists(), "a live holder's lock is not reaped");
    }

    #[test]
    fn a_dead_holders_lock_is_reaped_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        fs::write(&path, "1 1970-01-01T00:00:00Z\n").unwrap();
        // Treat anything older than zero as stale, so the reap happens at the
        // (short) deadline.
        let _held =
            Lock::acquire_within(&path, Duration::from_millis(100), Duration::ZERO).unwrap();
        assert!(path.exists(), "now held by us");
    }

    #[test]
    fn a_lock_released_by_another_thread_is_acquired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let held = Lock::acquire(&path).unwrap();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(held);
        });
        let _mine = Lock::acquire(&path).unwrap();
        t.join().unwrap();
    }
}
