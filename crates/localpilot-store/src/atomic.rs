//! File-write primitives: atomic whole-file writes and guarded line appends.
//!
//! Whole files are written to a sibling temporary file and then renamed over
//! the target ([`atomic_write`]). A crash mid-write leaves the temporary file
//! behind and the canonical file untouched, so an interrupted write can never
//! produce a half-written, corrupt record.
//!
//! Line-delimited logs grow through [`append_line`] instead: appending one
//! record does not rewrite (and therefore cannot re-corrupt or perpetuate) the
//! records already on disk, and a torn tail left by a crash is sealed off with
//! a newline before the next record so damage never bleeds into new entries.

use std::fs;
use std::io::Write;
use std::path::Path;

use crate::error::StoreError;

/// Write `bytes` to `path` atomically (temp-then-rename), creating parent
/// directories as needed.
///
/// # Errors
/// Returns [`StoreError::Io`] if a directory, write, or rename operation fails.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| StoreError::io(parent, e))?;
        }
    }

    let tmp = temp_sibling(path);
    fs::write(&tmp, bytes).map_err(|e| StoreError::io(&tmp, e))?;
    // `rename` replaces an existing destination atomically on all tier-1
    // platforms, so readers see either the old file or the complete new one.
    fs::rename(&tmp, path).map_err(|e| {
        // Best-effort cleanup; the error below is the one that matters.
        let _ = fs::remove_file(&tmp);
        StoreError::io(path, e)
    })
}

/// Append one newline-terminated record to a line-delimited log, creating
/// parent directories as needed.
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
/// Returns [`StoreError::Io`] if a directory, open, or write operation fails.
pub fn append_line(path: &Path, line: &str) -> Result<(), StoreError> {
    debug_assert!(!line.contains('\n'), "a log record must be a single line");
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| StoreError::io(parent, e))?;
        }
    }

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
        .map_err(|e| StoreError::io(path, e))
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

fn temp_sibling(path: &Path) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_roundtrips_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("file.txt");
        atomic_write(&path, b"hello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        assert!(!temp_sibling(&path).exists());
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
        // Simulate a crash after writing the temp file but before the rename:
        // the canonical file must still read back its committed contents.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        atomic_write(&path, b"committed").unwrap();
        std::fs::write(temp_sibling(&path), b"garbage-partial").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "committed");
    }
}
