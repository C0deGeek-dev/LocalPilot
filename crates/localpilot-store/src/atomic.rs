//! File-write primitives: atomic whole-file writes and guarded line appends.
//!
//! Whole files are written to a sibling temporary file, flushed to disk, and
//! then renamed over the target ([`atomic_write`]). A crash mid-write leaves
//! the temporary file behind and the canonical file untouched, so an
//! interrupted write can never produce a half-written, corrupt record.
//!
//! The temporary file has a name of its own, created exclusively: a fixed
//! `<file>.tmp` would overwrite and then delete an unrelated file of that name
//! beside the target, and would let two writers of one path trample each
//! other's half-written data.
//!
//! Line-delimited logs grow through [`append_line`] instead: appending one
//! record does not rewrite (and therefore cannot re-corrupt or perpetuate) the
//! records already on disk, and a torn tail left by a crash is sealed off with
//! a newline before the next record so damage never bleeds into new entries.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::error::StoreError;

/// Distinguishes this process's temporary files from each other.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
/// How many unique names to try before giving up on a temporary file.
const TEMP_ATTEMPTS: u32 = 100;
/// Windows refuses a rename while another process holds the target open (a
/// reader, or a concurrent replace): a transient sharing violation. Retry it
/// this many times, this far apart, before failing.
const RENAME_ATTEMPTS: u32 = 40;
const RENAME_PAUSE: Duration = Duration::from_millis(50);

/// Write `bytes` to `path` atomically (temp, flush, rename), creating parent
/// directories as needed.
///
/// # Errors
/// Returns [`StoreError::Io`] if a directory, write, flush or rename fails.
/// The temporary file is removed on every failure.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    ensure_parent(path)?;
    let (tmp, mut file) = create_temp_sibling(path)?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(StoreError::io(&tmp, e));
    }
    // `rename` replaces an existing destination atomically on all tier-1
    // platforms, so readers see either the old file or the complete new one.
    rename_retrying(&tmp, path).map_err(|e| {
        // Best-effort cleanup; the error below is the one that matters.
        let _ = fs::remove_file(&tmp);
        StoreError::io(path, e)
    })
}

/// Append one newline-terminated record to a line-delimited log, creating
/// parent directories as needed, and flush it to disk.
///
/// If the file's current tail is an unterminated line (a torn write from a
/// crash or power loss), a newline is inserted first so the damaged line stays
/// quarantined on its own physical line and the new record starts clean —
/// existing damage can never swallow a new record.
///
/// `line` must be a single serialized record without raw newlines (serialized
/// JSON never contains one).
///
/// # Errors
/// Returns [`StoreError::Io`] if a directory, open, write or flush fails.
pub fn append_line(path: &Path, line: &str) -> Result<(), StoreError> {
    debug_assert!(!line.contains('\n'), "a log record must be a single line");
    ensure_parent(path)?;

    let seal_torn_tail = match fs::metadata(path) {
        Ok(meta) if meta.len() > 0 => !ends_with_newline(path)?,
        _ => false,
    };

    let mut buf = String::with_capacity(line.len() + 2);
    if seal_torn_tail {
        buf.push('\n');
    }
    buf.push_str(line);
    buf.push('\n');

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| StoreError::io(path, e))?;
    file.write_all(buf.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|e| StoreError::io(path, e))
}

fn ensure_parent(path: &Path) -> Result<(), StoreError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| StoreError::io(parent, e))?;
        }
    }
    Ok(())
}

fn ends_with_newline(path: &Path) -> Result<bool, StoreError> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = fs::File::open(path).map_err(|e| StoreError::io(path, e))?;
    file.seek(SeekFrom::End(-1))
        .map_err(|e| StoreError::io(path, e))?;
    let mut byte = [0u8; 1];
    file.read_exact(&mut byte)
        .map_err(|e| StoreError::io(path, e))?;
    Ok(byte[0] == b'\n')
}

/// A new, exclusively created temporary file beside `path`, named
/// `.<file>.<pid>.<n>.tmp`. `create_new` fails rather than truncate anything
/// already there, so an existing file is never touched.
fn create_temp_sibling(path: &Path) -> Result<(PathBuf, fs::File), StoreError> {
    let base = path.file_name().unwrap_or_default();
    let mut last = io::Error::new(io::ErrorKind::AlreadyExists, "no free temporary name");
    for _ in 0..TEMP_ATTEMPTS {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut name = OsString::from(".");
        name.push(base);
        name.push(format!(".{}.{n}.tmp", std::process::id()));
        let tmp = path.with_file_name(name);
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = e,
            Err(e) => return Err(StoreError::io(&tmp, e)),
        }
    }
    Err(StoreError::io(path, last))
}

/// `ERROR_SHARING_VIOLATION`: Windows reports a destination held open without
/// delete sharing either as this or as access denied.
const SHARING_VIOLATION: i32 = 32;

fn is_transient_rename_error(e: &io::Error) -> bool {
    cfg!(windows)
        && (e.kind() == io::ErrorKind::PermissionDenied
            || e.raw_os_error() == Some(SHARING_VIOLATION))
}

fn rename_retrying(from: &Path, to: &Path) -> io::Result<()> {
    let mut attempt = 1;
    loop {
        match fs::rename(from, to) {
            Err(e) if is_transient_rename_error(&e) && attempt < RENAME_ATTEMPTS => {
                attempt += 1;
                std::thread::sleep(RENAME_PAUSE);
            }
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every leftover temporary file in `dir`.
    fn temps(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect()
    }

    #[test]
    fn write_then_read_roundtrips_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("file.txt");
        atomic_write(&path, b"hello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        assert!(temps(path.parent().unwrap()).is_empty());
    }

    #[test]
    fn overwrite_replaces_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
    }

    #[test]
    fn a_neighbouring_file_named_like_a_temp_is_never_touched() {
        // A real `<file>.tmp` next to the target used to be overwritten and
        // then renamed away by every write of `<file>`.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.txt");
        let neighbour = dir.path().join("report.txt.tmp");
        std::fs::write(&neighbour, b"the user's own file").unwrap();
        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();
        assert_eq!(
            std::fs::read_to_string(&neighbour).unwrap(),
            "the user's own file"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
    }

    #[test]
    fn concurrent_writers_of_one_path_never_share_a_temp_or_tear_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.json");
        let bodies: Vec<Vec<u8>> = (0..8u8).map(|i| vec![b'a' + i; 64 * 1024]).collect();
        std::thread::scope(|s| {
            for body in &bodies {
                let path = &path;
                s.spawn(move || {
                    for _ in 0..20 {
                        atomic_write(path, body).unwrap();
                    }
                });
            }
        });
        let got = std::fs::read(&path).unwrap();
        assert!(
            bodies.contains(&got),
            "the target holds exactly one writer's whole body"
        );
        assert!(
            temps(dir.path()).is_empty(),
            "no temporary file is left behind"
        );
    }

    /// Hold `path` open with no sharing at all, the way another process's
    /// reader or writer can on Windows, until `release` elapses.
    #[cfg(windows)]
    fn hold_exclusively(path: &Path, release: std::time::Duration) -> std::thread::JoinHandle<()> {
        use std::os::windows::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(path)
            .unwrap();
        std::thread::spawn(move || {
            std::thread::sleep(release);
            drop(file);
        })
    }

    #[cfg(windows)]
    #[test]
    fn a_destination_held_briefly_is_replaced_once_it_is_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("held.json");
        atomic_write(&path, b"old").unwrap();
        let holder = hold_exclusively(&path, Duration::from_millis(400));
        atomic_write(&path, b"new").unwrap();
        holder.join().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert!(temps(dir.path()).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn a_destination_held_past_the_retry_bound_fails_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("held.json");
        atomic_write(&path, b"old").unwrap();
        let window = RENAME_PAUSE * RENAME_ATTEMPTS + Duration::from_secs(2);
        let holder = hold_exclusively(&path, window);
        let started = std::time::Instant::now();
        let result = atomic_write(&path, b"new");
        let took = started.elapsed();
        holder.join().unwrap();
        assert!(
            result.is_err(),
            "a destination that stays held must fail, not hang"
        );
        assert!(
            took < window,
            "the failure is bounded by the retry budget: {took:?}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"old",
            "the old target is intact"
        );
        assert!(
            temps(dir.path()).is_empty(),
            "the temporary file is removed"
        );
    }

    #[test]
    fn append_line_creates_the_file_and_grows_it_without_rewriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("log.jsonl");
        append_line(&path, "{\"a\":1}").unwrap();
        append_line(&path, "{\"b\":2}").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"a\":1}\n{\"b\":2}\n"
        );
    }

    #[test]
    fn append_line_seals_a_torn_tail_so_the_new_record_starts_clean() {
        // Simulate a crash that left an unterminated line: the next append must
        // not glue onto the damaged bytes.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        append_line(&path, "{\"a\":1}").unwrap();
        let mut torn = std::fs::read_to_string(&path).unwrap();
        torn.push_str("{\"b\":2,\"text\":\"cut-off-mid-tok"); // no newline
        std::fs::write(&path, torn).unwrap();

        append_line(&path, "{\"c\":3}").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(
            lines,
            [
                "{\"a\":1}",
                "{\"b\":2,\"text\":\"cut-off-mid-tok",
                "{\"c\":3}"
            ]
        );
    }

    #[test]
    fn stray_temp_file_does_not_corrupt_the_canonical_file() {
        // Simulate a crash after writing a temp file but before the rename:
        // the canonical file must still read back its committed contents, and
        // the next write must not trip over the leftover.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        atomic_write(&path, b"committed").unwrap();
        let stray = dir
            .path()
            .join(format!(".file.txt.{}.0.tmp", std::process::id()));
        std::fs::write(&stray, b"garbage-partial").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "committed");
        atomic_write(&path, b"next").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "next");
        assert_eq!(std::fs::read(&stray).unwrap(), b"garbage-partial");
    }
}
