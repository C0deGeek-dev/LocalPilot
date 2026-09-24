//! Whole-file records: reads per the read contract (spec L-8) and atomic
//! replaces (spec L-7).

use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::error::MeshError;

/// Windows fails a read that lands while another process replaces the file
/// with a sharing violation. It is transient: retry this many times.
const READ_ATTEMPTS: u32 = 40;
const READ_PAUSE: Duration = Duration::from_millis(50);
const SHARING_VIOLATION: i32 = 32;

/// A whole file's bytes, or `None` when it is absent. A sharing violation is
/// retried; any other error is returned.
///
/// # Errors
/// [`MeshError::Io`] for anything but absence.
pub fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>, MeshError> {
    let mut attempt = 1;
    loop {
        match fs::read(path) {
            Ok(b) => return Ok(Some(b)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) if transient(&e) && attempt < READ_ATTEMPTS => {
                attempt += 1;
                std::thread::sleep(READ_PAUSE);
            }
            Err(e) => return Err(MeshError::io(path, e)),
        }
    }
}

fn transient(e: &io::Error) -> bool {
    cfg!(windows)
        && (e.kind() == io::ErrorKind::PermissionDenied
            || e.raw_os_error() == Some(SHARING_VIOLATION))
}

/// A whole-file JSON record: `None` when absent (its defined absent state),
/// an error when present but unparseable. It is never read as a default
/// (spec L-8): a corrupt pointer read as "no session" would let a new session
/// be published over live work.
///
/// # Errors
/// [`MeshError::Corrupt`] naming `shown` when the file is not the expected
/// JSON; [`MeshError::Io`] for read failures.
pub fn read_json<T: DeserializeOwned>(path: &Path, shown: &str) -> Result<Option<T>, MeshError> {
    let Some(bytes) = read_bytes(path)? else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| MeshError::Corrupt(format!("{shown} is not valid JSON ({e})")))
}

/// Replace a whole-file record atomically, as compact JSON plus a newline.
///
/// # Errors
/// [`MeshError::Serde`] or the store's I/O error.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), MeshError> {
    let mut body = serde_json::to_vec(value)?;
    body.push(b'\n');
    localpilot_store::atomic_write(path, &body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn absent_is_none_and_present_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("cursor").join("codex.json");
        assert!(read_json::<Value>(&p, "cursor").unwrap().is_none());
        write_json(&p, &json!({"peer_seq": 2})).unwrap();
        assert_eq!(
            read_json::<Value>(&p, "cursor").unwrap(),
            Some(json!({"peer_seq": 2}))
        );
        assert!(fs::read_to_string(&p).unwrap().ends_with("}\n"));
    }

    #[test]
    fn an_unparseable_record_is_corrupt_never_absent() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("active.json");
        fs::write(&p, "{").unwrap();
        let err = read_json::<Value>(&p, "active.json").unwrap_err();
        assert!(
            matches!(err, MeshError::Corrupt(ref m) if m.starts_with("active.json is not valid JSON")),
            "{err}"
        );
    }
}
