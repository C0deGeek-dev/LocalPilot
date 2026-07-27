//! Making the resolved version the one that actually runs.
//!
//! The cache decides *which* version should run, but nothing inside it puts a
//! binary where a shell will find it. This module closes that gap: one file per
//! tool in a `bin` directory, refreshed from the resolver whenever the cache
//! changes (install, pin, rollback). Put that directory on `PATH` once and every
//! later version switch is invisible to the user.
//!
//! **A copy, not a link.** Symlinks need a privilege or developer mode on
//! Windows, and a hard link cannot cross volumes; a copy works everywhere and
//! costs one file per tool. The version directories remain the source of truth —
//! `bin/` is a pointer that can be rebuilt from them at any time.
//!
//! **Replacing it is rename-then-copy**, in that order, because Windows refuses
//! to overwrite a running executable but *does* allow renaming one out of the
//! way. The displaced file is swept on the next activation, once whatever was
//! running it has exited.

use std::path::{Path, PathBuf};

use crate::cache::Cache;
use crate::error::DistError;
use crate::resolve::resolve;
use crate::version::Version;

/// Directory holding the `PATH`-visible executables.
const BIN_DIR: &str = "bin";
/// Extension marking a displaced executable awaiting sweep.
const DISPLACED_SUFFIX: &str = ".displaced";

/// The `PATH`-visible directory for a cache rooted at `root`.
#[must_use]
pub fn bin_dir(root: &Path) -> PathBuf {
    root.join(BIN_DIR)
}

/// The file name a tool is published under on this platform.
#[must_use]
pub fn executable_name(tool: &str) -> String {
    if cfg!(windows) {
        format!("{tool}.exe")
    } else {
        tool.to_string()
    }
}

/// Copy `source` to `<bin_dir>/<tool>`, displacing any existing file first.
///
/// Returns the path now on `PATH`. The new bytes are written to a temporary name
/// and renamed into place, so an interrupted copy never leaves a half-written
/// executable where a shell would run it.
///
/// # Errors
/// Returns [`DistError::Io`] when the directory cannot be created or the copy or
/// rename fails. On failure the previously active executable is restored.
pub fn place(bin_dir: &Path, tool: &str, source: &Path) -> Result<PathBuf, DistError> {
    std::fs::create_dir_all(bin_dir).map_err(DistError::io)?;
    sweep_displaced(bin_dir);

    let dest = bin_dir.join(executable_name(tool));
    let incoming = bin_dir.join(format!("{}.incoming", executable_name(tool)));

    // Write the new bytes first. If the download or the cache is somehow
    // unreadable, the currently active executable has not been touched.
    let _ = std::fs::remove_file(&incoming);
    std::fs::copy(source, &incoming).map_err(DistError::io)?;

    // Move the old one aside rather than overwriting it: on Windows the file may
    // be executing right now, and rename is the one operation that is still
    // permitted in that state.
    let displaced = if dest.exists() {
        let displaced = bin_dir.join(format!("{}{DISPLACED_SUFFIX}", executable_name(tool)));
        let _ = std::fs::remove_file(&displaced);
        match std::fs::rename(&dest, &displaced) {
            Ok(()) => Some(displaced),
            Err(error) => {
                let _ = std::fs::remove_file(&incoming);
                return Err(DistError::io(error));
            }
        }
    } else {
        None
    };

    match std::fs::rename(&incoming, &dest) {
        Ok(()) => {
            // Best effort: the displaced copy is unlinkable only while a process
            // still holds it, and the next activation sweeps it.
            if let Some(displaced) = displaced {
                let _ = std::fs::remove_file(displaced);
            }
            Ok(dest)
        }
        Err(error) => {
            // Put the previous executable back; a failed update must not leave
            // the user with no working binary at all.
            if let Some(displaced) = displaced {
                let _ = std::fs::rename(&displaced, &dest);
            }
            let _ = std::fs::remove_file(&incoming);
            Err(DistError::io(error))
        }
    }
}

/// Refresh `<bin_dir>/<tool>` to whatever the resolver currently chooses.
///
/// `bin_dir` is passed rather than derived from the cache so that a set of tools
/// can share one directory: a user adds a single entry to `PATH`, not one per
/// tool. The caches stay separate — only the pointer is shared.
///
/// Returns the activated path, or `None` when the resolver's answer is "the build
/// that is already running" — there is no cached payload to point at in that
/// case, and inventing one would copy a binary out of wherever it happens to live.
///
/// # Errors
/// Returns [`DistError::Io`] when the copy or rename fails.
pub fn activate(
    cache: &Cache,
    bin_dir: &Path,
    tool: &str,
    running: &Version,
) -> Result<Option<PathBuf>, DistError> {
    let resolution = resolve(cache, running);
    let Some(executable) = resolution.executable else {
        return Ok(None);
    };
    place(bin_dir, tool, &executable).map(Some)
}

/// Remove executables displaced by an earlier activation. Failure is ignored:
/// a displaced file that is still running simply waits for the next sweep.
fn sweep_displaced(bin_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(bin_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(DISPLACED_SUFFIX))
        {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, body).expect("write");
    }

    #[test]
    fn place_creates_the_bin_directory_and_copies_the_payload() {
        let temp = tempfile::tempdir().expect("temp");
        let source = temp.path().join("payload");
        write(&source, "v1");

        let bin = temp.path().join("bin");
        let placed = place(&bin, "tool", &source).expect("place");

        assert_eq!(placed, bin.join(executable_name("tool")));
        assert_eq!(std::fs::read_to_string(&placed).expect("read"), "v1");
    }

    #[test]
    fn placing_again_replaces_the_previous_executable() {
        let temp = tempfile::tempdir().expect("temp");
        let bin = temp.path().join("bin");

        let first = temp.path().join("first");
        write(&first, "v1");
        place(&bin, "tool", &first).expect("first");

        let second = temp.path().join("second");
        write(&second, "v2");
        let placed = place(&bin, "tool", &second).expect("second");

        assert_eq!(std::fs::read_to_string(&placed).expect("read"), "v2");
    }

    #[test]
    fn a_displaced_executable_is_swept_on_the_next_activation() {
        let temp = tempfile::tempdir().expect("temp");
        let bin = temp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("bin");

        // Simulate a previous activation that could not unlink the old binary
        // because it was still running.
        let stale = bin.join(format!("{}{DISPLACED_SUFFIX}", executable_name("tool")));
        write(&stale, "old");

        let source = temp.path().join("payload");
        write(&source, "new");
        place(&bin, "tool", &source).expect("place");

        assert!(!stale.exists(), "the displaced file should have been swept");
    }

    #[test]
    fn a_failed_copy_leaves_the_active_executable_untouched() {
        let temp = tempfile::tempdir().expect("temp");
        let bin = temp.path().join("bin");

        let good = temp.path().join("good");
        write(&good, "working");
        place(&bin, "tool", &good).expect("place");

        // A source that does not exist stands in for any unreadable payload.
        let missing = temp.path().join("does-not-exist");
        assert!(place(&bin, "tool", &missing).is_err());

        let dest = bin.join(executable_name("tool"));
        assert_eq!(
            std::fs::read_to_string(&dest).expect("read"),
            "working",
            "the previously active executable must survive a failed placement"
        );
    }

    #[test]
    fn activate_points_at_the_resolved_version() {
        let temp = tempfile::tempdir().expect("temp");
        let cache = Cache::new(temp.path().join("tool"));
        seed(&cache, "1.0.0", "one");
        seed(&cache, "2.0.0", "two");

        let running = Version::parse("0.9.0").expect("version");
        let bin = temp.path().join("shared-bin");
        let placed = activate(&cache, &bin, "tool", &running)
            .expect("activate")
            .expect("a cached version should have been chosen");

        assert_eq!(
            std::fs::read_to_string(&placed).expect("read"),
            "two",
            "the newest cached version should be the one on PATH"
        );
    }

    #[test]
    fn activate_reports_nothing_to_do_when_the_running_build_wins() {
        let temp = tempfile::tempdir().expect("temp");
        let cache = Cache::new(temp.path().join("tool"));

        let running = Version::parse("2.0.0").expect("version");
        let bin = temp.path().join("shared-bin");
        let placed = activate(&cache, &bin, "tool", &running).expect("activate");

        assert!(
            placed.is_none(),
            "with an empty cache there is no payload to point at"
        );
    }

    #[test]
    fn activate_follows_a_pin_rather_than_the_newest() {
        let temp = tempfile::tempdir().expect("temp");
        let cache = Cache::new(temp.path().join("tool"));
        seed(&cache, "1.0.0", "one");
        seed(&cache, "2.0.0", "two");
        let pinned = Version::parse("1.0.0").expect("version");
        cache.set_pin(&pinned).expect("pin");

        let running = Version::parse("2.0.0").expect("version");
        let bin = temp.path().join("shared-bin");
        let placed = activate(&cache, &bin, "tool", &running)
            .expect("activate")
            .expect("the pinned version is installed");

        assert_eq!(
            std::fs::read_to_string(&placed).expect("read"),
            "one",
            "a pin must win over the newest cached version"
        );
    }

    /// Install `version` into `cache` with `body` as the executable's contents.
    fn seed(cache: &Cache, version: &str, body: &str) {
        let version = Version::parse(version).expect("version");
        let staged = cache.stage(&version).expect("stage");
        let executable = executable_name("tool");
        write(&staged.join(&executable), body);
        let marker = crate::cache::InstallMarker {
            marker_version: crate::cache::MARKER_VERSION,
            version: version.to_dir_name(),
            target: "test-target".to_string(),
            sha256: "0".repeat(64),
            executable,
        };
        cache.commit(&version, &staged, &marker).expect("commit");
    }
}
