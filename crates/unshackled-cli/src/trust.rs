//! Per-folder trust.
//!
//! The interactive REPL asks once, on first entry into a workspace folder,
//! whether the folder is trusted. The answer is remembered in a small list under
//! the user config directory so the prompt does not reappear for that folder.
//! Trust is a convenience gate, not a security boundary — the permission engine
//! still governs every effect — so failures here are swallowed rather than
//! surfaced.

use std::io::Write;
use std::path::{Path, PathBuf};

use unshackled_config::user_config_path;

/// The file that lists trusted folders, one absolute path per line. It sits next
/// to the user config file. Returns `None` when no config base directory exists.
fn store_path() -> Option<PathBuf> {
    user_config_path().map(|config| config.with_file_name("trusted-folders.txt"))
}

/// A stable string key for `path`, canonicalized where possible so symlinks and
/// relative spellings of the same folder compare equal.
fn key(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Whether `cwd` has been trusted before.
#[must_use]
pub fn is_trusted(cwd: &Path) -> bool {
    let Some(path) = store_path() else {
        return false;
    };
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return false;
    };
    let target = key(cwd);
    contents.lines().any(|line| line.trim() == target)
}

/// Record `cwd` as trusted. A no-op if it is already recorded or if no config
/// directory is available.
pub fn remember(cwd: &Path) {
    let Some(path) = store_path() else {
        return;
    };
    if is_trusted(cwd) {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let entry = format!("{}\n", key(cwd));
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = file.write_all(entry.as_bytes());
    }
}
