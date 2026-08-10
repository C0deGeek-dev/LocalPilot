//! Per-folder trust.
//!
//! The interactive host asks once, on first entry into a workspace folder,
//! whether the folder is trusted. It remembers an affirmative answer and also
//! offers a session-only affirmative choice.
//! Persistent answers live in a small list under the user config directory so
//! the prompt does not reappear for that folder.
//! Trust is a convenience gate, not a security boundary — the permission engine
//! still governs every effect — so an interactive failure to persist is logged
//! at warn level (it would otherwise silently re-prompt every session) rather
//! than treated as fatal. The scriptable `localpilot trust` surface, by
//! contrast, returns typed results so an operator can tell a broken store from a
//! genuinely untrusted folder.

use std::io::Write;
use std::path::{Path, PathBuf};

use localpilot_config::user_config_path;
use localpilot_sandbox::Profile;

/// The file that lists trusted folders, one absolute path per line. It sits next
/// to the user config file. Returns `None` when no config base directory exists.
pub(crate) fn store_path() -> Option<PathBuf> {
    user_config_path().map(|config| config.with_file_name("trusted-folders.txt"))
}

/// A stable string key for `path`, canonicalized where possible so symlinks and
/// relative spellings of the same folder compare equal. Lossy fallback: used by
/// the best-effort interactive `remember` path only.
fn key(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// The evaluated trust of a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trust {
    Trusted,
    Untrusted,
}

/// The result of recording a folder as trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AddOutcome {
    Added,
    AlreadyPresent,
}

/// The result of removing a folder's trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoveOutcome {
    Removed,
    Absent,
}

/// A trust-store operation that could not be evaluated. Kept distinct from a
/// genuine `Untrusted` answer so the diagnostic surface never reports a broken
/// or unreadable store as a confident decision.
#[derive(Debug)]
pub(crate) enum TrustError {
    /// No user config base directory exists, so the store path is unknown.
    ConfigBaseUnavailable,
    /// The target path is not an existing directory, or cannot be resolved.
    InvalidTarget(String),
    /// A real read/write/permission/encoding error touching the store.
    Persistence(std::io::Error),
}

impl std::fmt::Display for TrustError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConfigBaseUnavailable => write!(
                formatter,
                "no user config directory is available to store workspace trust"
            ),
            Self::InvalidTarget(reason) => write!(formatter, "{reason}"),
            Self::Persistence(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for TrustError {}

/// A stable comparison key: two entries that differ only by a Windows verbatim
/// (`\\?\`) prefix name the same folder, so a stale non-verbatim entry still
/// matches a canonical one on removal.
fn match_key(entry: &str) -> &str {
    entry.strip_prefix(r"\\?\").unwrap_or(entry)
}

/// The canonical key of an existing directory. Fails (`InvalidTarget`) when the
/// path does not exist, cannot be canonicalized, or is not a directory. Used by
/// `add` and `status`, which require a concrete folder.
pub(crate) fn canonical_key(dir: &Path) -> Result<String, TrustError> {
    let canonical = std::fs::canonicalize(dir)
        .map_err(|error| TrustError::InvalidTarget(format!("{}: {error}", dir.display())))?;
    if !canonical.is_dir() {
        return Err(TrustError::InvalidTarget(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    Ok(canonical.to_string_lossy().into_owned())
}

/// The key to remove for `dir`: canonical when the directory still exists, and a
/// stable lexical-absolute fallback when it has been deleted (so a stale entry
/// can still be cleaned up). An existing non-directory target is rejected. Also
/// what the command prints, so the reported folder is the entry actually matched.
pub(crate) fn removal_key(dir: &Path) -> Result<String, TrustError> {
    match std::fs::symlink_metadata(dir) {
        Ok(_) => canonical_key(dir),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::path::absolute(dir)
            .map(|absolute| absolute.to_string_lossy().into_owned())
            .map_err(|error| TrustError::InvalidTarget(format!("{}: {error}", dir.display()))),
        Err(error) => Err(TrustError::Persistence(error)),
    }
}

/// The trimmed, non-empty entries in the store. A missing store is an empty
/// list; only a missing store is treated as empty — every other I/O error is
/// surfaced, never swallowed.
fn read_entries(store: &Path) -> Result<Vec<String>, TrustError> {
    match std::fs::read_to_string(store) {
        Ok(contents) => Ok(contents
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(TrustError::Persistence(error)),
    }
}

/// The result-returning trust query behind `status` and `doctor`. Distinguishes
/// an evaluation failure (`Err`) from a genuine `Untrusted` answer.
pub(crate) fn is_trusted_result_in(dir: &Path, store: &Path) -> Result<Trust, TrustError> {
    let target = canonical_key(dir)?;
    let entries = read_entries(store)?;
    let trusted = entries
        .iter()
        .any(|entry| match_key(entry) == match_key(&target));
    Ok(if trusted {
        Trust::Trusted
    } else {
        Trust::Untrusted
    })
}

/// Whether `cwd` has been trusted before. Fail-closed: any evaluation error
/// (unreadable store, unresolvable path, no config base) reads as untrusted, so
/// the interactive gate never opens on a broken store.
#[must_use]
pub fn is_trusted(cwd: &Path) -> bool {
    match store_path() {
        Some(store) => matches!(is_trusted_result_in(cwd, &store), Ok(Trust::Trusted)),
        None => false,
    }
}

/// Whether entering `cwd` under `profile` must raise the trust prompt. The
/// single source of truth for the three host startup sites, all of which are
/// compiled only with the interactive `tui` feature.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
#[must_use]
pub fn prompt_required(profile: Profile, cwd: &Path) -> bool {
    prompt_required_for(profile, is_trusted(cwd))
}

/// The pure prompt decision: a prompting profile (anything but
/// Bypass/Unrestricted) over an untrusted folder needs the prompt. Split from the
/// store read so the full profile × trust table is testable without the real
/// user store.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
#[must_use]
fn prompt_required_for(profile: Profile, trusted: bool) -> bool {
    !matches!(profile, Profile::Bypass | Profile::Unrestricted) && !trusted
}

/// Record `dir` as trusted in `store`. Idempotent. Fails on an invalid target or
/// a real store error.
pub(crate) fn add_in(dir: &Path, store: &Path) -> Result<AddOutcome, TrustError> {
    let target = canonical_key(dir)?;
    let entries = read_entries(store)?;
    if entries
        .iter()
        .any(|entry| match_key(entry) == match_key(&target))
    {
        return Ok(AddOutcome::AlreadyPresent);
    }
    if let Some(parent) = store.parent() {
        std::fs::create_dir_all(parent).map_err(TrustError::Persistence)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(store)
        .map_err(TrustError::Persistence)?;
    writeln!(file, "{target}").map_err(TrustError::Persistence)?;
    Ok(AddOutcome::Added)
}

/// Remove every entry naming `dir` from `store`, rewriting atomically. Removing
/// an absent folder is a clean no-op.
pub(crate) fn remove_in(dir: &Path, store: &Path) -> Result<RemoveOutcome, TrustError> {
    let target = removal_key(dir)?;
    let entries = read_entries(store)?;
    let kept: Vec<&str> = entries
        .iter()
        .map(String::as_str)
        .filter(|entry| match_key(entry) != match_key(&target))
        .collect();
    if kept.len() == entries.len() {
        return Ok(RemoveOutcome::Absent);
    }
    rewrite_atomic(store, &kept)?;
    Ok(RemoveOutcome::Removed)
}

/// The stable-sorted, de-duplicated trusted folders in `store`.
pub(crate) fn list_in(store: &Path) -> Result<Vec<String>, TrustError> {
    let mut entries = read_entries(store)?;
    entries.sort();
    entries.dedup();
    Ok(entries)
}

/// Replace the store's contents with `lines`, via a uniquely-named
/// same-directory temp file and an atomic same-volume persist. `NamedTempFile`
/// avoids a predictable sibling that could collide across processes or follow a
/// pre-existing path, and auto-cleans on any failure, so the live store is only
/// ever replaced by a fully-written file and is left intact on error.
fn rewrite_atomic(store: &Path, lines: &[&str]) -> Result<(), TrustError> {
    let parent = store.parent().ok_or_else(|| {
        TrustError::Persistence(std::io::Error::new(
            std::io::ErrorKind::Other,
            "trust store has no parent directory",
        ))
    })?;
    let mut staged = tempfile::NamedTempFile::new_in(parent).map_err(TrustError::Persistence)?;
    for line in lines {
        writeln!(staged, "{line}").map_err(TrustError::Persistence)?;
    }
    staged.flush().map_err(TrustError::Persistence)?;
    staged
        .as_file()
        .sync_all()
        .map_err(TrustError::Persistence)?;
    staged
        .persist(store)
        .map_err(|error| TrustError::Persistence(error.error))?;
    Ok(())
}

/// Record `cwd` as trusted. A best-effort no-op if it is already recorded or if
/// no config directory is available; the interactive dialog uses this, so a
/// persistence failure is logged, not fatal. Scriptable persistence goes through
/// `add_in`, which returns errors.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
pub fn remember(cwd: &Path) {
    let Some(path) = store_path() else {
        return;
    };
    remember_in(cwd, &path);
}

fn remember_in(cwd: &Path, path: &Path) {
    if is_trusted_in(cwd, path) {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let entry = format!("{}\n", key(cwd));
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| file.write_all(entry.as_bytes()));
    if let Err(error) = result {
        // Not a security boundary, but a silent failure means the user is
        // re-prompted to trust this folder every session with no way to see why.
        tracing::warn!(
            target: "localpilot::trust",
            path = %path.display(),
            %error,
            "could not persist workspace trust; you may be asked to trust this folder again next session"
        );
    }
}

fn is_trusted_in(cwd: &Path, path: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    let target = key(cwd);
    contents
        .lines()
        .any(|line| match_key(line.trim()) == match_key(&target))
}

#[cfg(all(test, feature = "tui"))]
pub(crate) fn remember_in_test_store(cwd: &Path, path: &Path) {
    remember_in(cwd, path);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[test]
    fn query_distinguishes_missing_present_absent_and_unreadable() {
        let project = temp_dir();
        let home = temp_dir();
        let store = home.path().join("trusted-folders.txt");

        // Missing store -> Untrusted, not an error.
        assert_eq!(
            is_trusted_result_in(project.path(), &store).unwrap(),
            Trust::Untrusted
        );

        // Recorded -> Trusted.
        assert_eq!(add_in(project.path(), &store).unwrap(), AddOutcome::Added);
        assert_eq!(
            is_trusted_result_in(project.path(), &store).unwrap(),
            Trust::Trusted
        );

        // A different folder in the same store -> Untrusted.
        let other = temp_dir();
        assert_eq!(
            is_trusted_result_in(other.path(), &store).unwrap(),
            Trust::Untrusted
        );

        // An unreadable store (a directory where a file is expected) is a real
        // error, never silently Untrusted.
        let unreadable = home.path().join("as-a-dir");
        std::fs::create_dir_all(&unreadable).expect("mkdir");
        assert!(matches!(
            is_trusted_result_in(project.path(), &unreadable),
            Err(TrustError::Persistence(_))
        ));
    }

    #[test]
    fn add_is_idempotent_and_rejects_a_non_directory() {
        let project = temp_dir();
        let home = temp_dir();
        let store = home.path().join("trusted-folders.txt");

        assert_eq!(add_in(project.path(), &store).unwrap(), AddOutcome::Added);
        assert_eq!(
            add_in(project.path(), &store).unwrap(),
            AddOutcome::AlreadyPresent
        );

        // A file target is invalid.
        let file = home.path().join("a-file");
        std::fs::write(&file, b"x").expect("write");
        assert!(matches!(
            add_in(&file, &store),
            Err(TrustError::InvalidTarget(_))
        ));
    }

    #[test]
    fn remove_deletes_every_matching_line_and_reports_absent() {
        let project = temp_dir();
        let home = temp_dir();
        let store = home.path().join("trusted-folders.txt");

        // A legacy store with the same folder duplicated.
        let entry = canonical_key(project.path()).unwrap();
        std::fs::write(&store, format!("{entry}\n{entry}\n")).expect("seed");
        assert_eq!(
            is_trusted_result_in(project.path(), &store).unwrap(),
            Trust::Trusted
        );

        assert_eq!(
            remove_in(project.path(), &store).unwrap(),
            RemoveOutcome::Removed
        );
        // Both duplicates gone -> a single remove flips status to Untrusted.
        assert_eq!(
            is_trusted_result_in(project.path(), &store).unwrap(),
            Trust::Untrusted
        );

        // Removing an absent folder is a clean no-op.
        assert_eq!(
            remove_in(project.path(), &store).unwrap(),
            RemoveOutcome::Absent
        );
    }

    #[test]
    fn remove_tolerates_a_deleted_directory_entry() {
        let home = temp_dir();
        let store = home.path().join("trusted-folders.txt");
        let ephemeral = temp_dir();
        let path = ephemeral.path().to_path_buf();

        // A stale entry for a folder that no longer exists, in the absolute form
        // the fallback produces.
        let absolute = std::path::absolute(&path)
            .expect("absolute")
            .to_string_lossy()
            .into_owned();
        std::fs::write(&store, format!("{absolute}\n")).expect("seed");
        drop(ephemeral);

        let outcome = remove_in(&path, &store).expect("remove stale");
        assert_eq!(outcome, RemoveOutcome::Removed);
        assert!(list_in(&store).unwrap().is_empty());
    }

    #[test]
    fn prompt_required_covers_the_full_profile_and_trust_table() {
        // A prompting profile prompts only over an untrusted folder; the
        // always-approved profiles never prompt. The complete table, on the pure
        // decision so both the trusted and untrusted branches are exercised
        // without the real user store.
        assert!(prompt_required_for(Profile::Default, false));
        assert!(!prompt_required_for(Profile::Default, true));
        assert!(!prompt_required_for(Profile::Bypass, false));
        assert!(!prompt_required_for(Profile::Bypass, true));
        assert!(!prompt_required_for(Profile::Unrestricted, false));
        assert!(!prompt_required_for(Profile::Unrestricted, true));
    }

    #[test]
    fn list_is_sorted_and_deduplicated() {
        let home = temp_dir();
        let store = home.path().join("trusted-folders.txt");
        std::fs::write(&store, "b\na\nb\n\n").expect("seed");
        assert_eq!(
            list_in(&store).unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
    }
}
