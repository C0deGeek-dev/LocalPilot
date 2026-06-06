//! Atomic file writes.
//!
//! Every persisted file is written to a sibling temporary file and then renamed
//! over the target. A crash mid-write leaves the temporary file behind and the
//! canonical file untouched, so an interrupted write can never produce a
//! half-written, corrupt record.

use std::fs;
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
