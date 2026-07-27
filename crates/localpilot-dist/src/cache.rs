//! The version-keyed install cache.
//!
//! Every installed version lives in its own directory, named by its version, so
//! **switching versions is a rename and rollback is free**. The running binary is
//! never overwritten in place — that is the only approach that behaves the same
//! on Windows (where you cannot replace a running executable) as on Unix, and it
//! is what makes an interrupted update leave the previous version working.
//!
//! This module does **not** verify anything. It *records* what a caller already
//! verified, in a marker file beside the payload. Verification belongs with the
//! download, where the bytes are in hand; re-hashing a tree on every resolve
//! would make startup pay for a check that install already did.
//!
//! A version becomes visible to the resolver only when its marker exists. Install
//! writes the marker **last**, inside a staging directory, and the staging
//! directory is renamed into place atomically — so a torn install is never
//! resolvable.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::DistError;
use crate::version::Version;

/// The marker written beside an installed payload. Its presence is what makes a
/// cached version resolvable; its contents are what a consistency check reads.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct InstallMarker {
    /// Marker format version, so a future change is detectable rather than
    /// silently misread.
    pub marker_version: u32,
    /// The version installed here. Must match the directory name.
    pub version: String,
    /// The target triple this payload was built for.
    pub target: String,
    /// The digest the caller verified before handing the payload over. Recorded,
    /// not re-checked on the hot path.
    pub sha256: String,
    /// The executable's file name inside this directory.
    pub executable: String,
}

/// The marker format this build writes and understands.
pub const MARKER_VERSION: u32 = 1;
/// File name of the marker inside a version directory.
const MARKER_FILE: &str = ".install.json";
/// Directory holding the per-version installs.
const VERSIONS_DIR: &str = "versions";
/// File holding a pinned version, when one is set.
const PIN_FILE: &str = "pin";

/// A cached version the resolver may choose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedVersion {
    pub version: Version,
    pub marker: InstallMarker,
    pub dir: PathBuf,
}

impl CachedVersion {
    /// Full path to the executable this version installed.
    #[must_use]
    pub fn executable(&self) -> PathBuf {
        self.dir.join(&self.marker.executable)
    }
}

/// The on-disk cache for one tool.
#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

impl Cache {
    /// A cache rooted at `root` (the tool's own directory).
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The platform's per-user data directory for `tool`, when the platform
    /// reports one. Mirrors the config crate's base-directory choice so a user
    /// has one place to look, not two conventions.
    #[must_use]
    pub fn default_root(tool: &str) -> Option<PathBuf> {
        let base = if cfg!(windows) {
            std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
        } else {
            std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
                })
        };
        base.map(|base| base.join(tool))
    }

    /// The directory a given version installs into.
    #[must_use]
    pub fn version_dir(&self, version: &Version) -> PathBuf {
        self.root.join(VERSIONS_DIR).join(version.to_dir_name())
    }

    /// Every installed version that carries a coherent marker, newest first.
    ///
    /// A directory whose marker is missing, unreadable, of an unknown format
    /// version, or disagrees with its own directory name is **skipped** — a torn
    /// or foreign install must not become resolvable just because it exists.
    #[must_use]
    pub fn installed(&self) -> Vec<CachedVersion> {
        let versions_dir = self.root.join(VERSIONS_DIR);
        let Ok(entries) = std::fs::read_dir(&versions_dir) else {
            return Vec::new();
        };
        let mut found: Vec<CachedVersion> = entries
            .flatten()
            .filter_map(|entry| {
                let dir = entry.path();
                if !dir.is_dir() {
                    return None;
                }
                let name = dir.file_name()?.to_str()?;
                let version = Version::parse(name)?;
                let marker = read_marker(&dir)?;
                // The marker must agree with where it was found; a mismatch means
                // the tree was moved or hand-edited, and guessing is worse than
                // ignoring it.
                if marker.marker_version != MARKER_VERSION || marker.version != name {
                    return None;
                }
                if !dir.join(&marker.executable).is_file() {
                    return None;
                }
                Some(CachedVersion {
                    version,
                    marker,
                    dir,
                })
            })
            .collect();
        found.sort_by(|a, b| b.version.key().cmp(&a.version.key()));
        found
    }

    /// The newest installed version, if any.
    #[must_use]
    pub fn newest(&self) -> Option<CachedVersion> {
        self.installed().into_iter().next()
    }

    /// Look up one installed version.
    #[must_use]
    pub fn get(&self, version: &Version) -> Option<CachedVersion> {
        self.installed()
            .into_iter()
            .find(|candidate| candidate.version == *version)
    }

    /// A staging directory for a caller to populate before [`Cache::commit`].
    ///
    /// The name is unique per process and attempt, so two concurrent installs of
    /// the same version cannot write into each other's tree.
    ///
    /// # Errors
    /// Returns [`DistError::Io`] when the directory cannot be created.
    pub fn stage(&self, version: &Version) -> Result<PathBuf, DistError> {
        let versions = self.root.join(VERSIONS_DIR);
        std::fs::create_dir_all(&versions).map_err(DistError::io)?;
        let unique = format!(
            ".staging-{}-{}-{}",
            version.to_dir_name(),
            std::process::id(),
            next_attempt()
        );
        let dir = versions.join(unique);
        // A leftover staging directory from a killed run is removed rather than
        // reused: its contents are unknown.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(DistError::io)?;
        Ok(dir)
    }

    /// Publish a staged payload as an installed version.
    ///
    /// Writes the marker **inside the staging directory first**, then renames the
    /// whole directory into place. The rename is the commit point: before it the
    /// version does not exist, after it the version is complete. There is no
    /// window in which a half-written version is resolvable.
    ///
    /// If another process won the race and the version already exists, the
    /// staged copy is discarded and the existing install is kept — installing the
    /// same version twice is not an error.
    ///
    /// # Errors
    /// Returns [`DistError::Io`] when the marker cannot be written or the rename
    /// fails for a reason other than the target already existing.
    pub fn commit(
        &self,
        version: &Version,
        staged: &Path,
        marker: &InstallMarker,
    ) -> Result<PathBuf, DistError> {
        if marker.version != version.to_dir_name() {
            return Err(DistError::Invalid(format!(
                "marker says version {:?} but is being committed as {:?}",
                marker.version,
                version.to_dir_name()
            )));
        }
        if !staged.join(&marker.executable).is_file() {
            return Err(DistError::Invalid(format!(
                "staged payload has no executable named {:?}",
                marker.executable
            )));
        }
        let body = serde_json::to_vec_pretty(marker).map_err(|e| DistError::Io(e.to_string()))?;
        std::fs::write(staged.join(MARKER_FILE), body).map_err(DistError::io)?;

        let target = self.version_dir(version);
        match std::fs::rename(staged, &target) {
            Ok(()) => Ok(target),
            Err(_) if target.is_dir() => {
                // Someone else installed it first. Theirs is as good as ours.
                let _ = std::fs::remove_dir_all(staged);
                Ok(target)
            }
            Err(error) => Err(DistError::io(error)),
        }
    }

    /// Remove installed versions beyond `keep`, never touching `protected`.
    ///
    /// Returns the versions actually removed, so a caller can report the sweep
    /// instead of silently reclaiming disk.
    ///
    /// # Errors
    /// Never fails the sweep for one undeletable directory — it is skipped and
    /// omitted from the returned list.
    pub fn sweep(&self, keep: usize, protected: &[Version]) -> Vec<Version> {
        let installed = self.installed();
        let mut removed = Vec::new();
        for (index, cached) in installed.iter().enumerate() {
            let is_protected = protected.contains(&cached.version);
            if index < keep || is_protected {
                continue;
            }
            if std::fs::remove_dir_all(&cached.dir).is_ok() {
                removed.push(cached.version.clone());
            }
        }
        // Leftover staging directories are swept too: a killed install would
        // otherwise leak a tree that nothing ever reads.
        if let Ok(entries) = std::fs::read_dir(self.root.join(VERSIONS_DIR)) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir()
                    && path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(".staging-"))
                {
                    let _ = std::fs::remove_dir_all(&path);
                }
            }
        }
        removed
    }

    /// The pinned version, when one is set and parseable.
    #[must_use]
    pub fn pin(&self) -> Option<Version> {
        let text = std::fs::read_to_string(self.root.join(PIN_FILE)).ok()?;
        Version::parse(&text)
    }

    /// Pin a version, so the resolver stops preferring the newest.
    ///
    /// # Errors
    /// Returns [`DistError::Io`] when the pin cannot be written.
    pub fn set_pin(&self, version: &Version) -> Result<(), DistError> {
        std::fs::create_dir_all(&self.root).map_err(DistError::io)?;
        std::fs::write(self.root.join(PIN_FILE), version.to_dir_name()).map_err(DistError::io)
    }

    /// Remove any pin. Removing an absent pin is not an error.
    ///
    /// # Errors
    /// Returns [`DistError::Io`] when an existing pin cannot be removed.
    pub fn clear_pin(&self) -> Result<(), DistError> {
        match std::fs::remove_file(self.root.join(PIN_FILE)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(DistError::io(error)),
        }
    }
}

fn read_marker(dir: &Path) -> Option<InstallMarker> {
    let body = std::fs::read_to_string(dir.join(MARKER_FILE)).ok()?;
    serde_json::from_str(&body).ok()
}

/// Monotonic per-process counter, so two staging directories in one process
/// cannot collide.
fn next_attempt() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = if cfg!(windows) { "tool.exe" } else { "tool" };

    fn marker(version: &str) -> InstallMarker {
        InstallMarker {
            marker_version: MARKER_VERSION,
            version: version.to_string(),
            target: "x86_64-pc-windows-msvc".to_string(),
            sha256: "0".repeat(64),
            executable: EXE.to_string(),
        }
    }

    fn install(cache: &Cache, version: &str) -> PathBuf {
        let parsed = Version::parse(version).expect("parses");
        let staged = cache.stage(&parsed).expect("stage");
        std::fs::write(staged.join(EXE), b"binary").expect("write payload");
        cache
            .commit(&parsed, &staged, &marker(version))
            .expect("commit")
    }

    fn cache() -> (tempfile::TempDir, Cache) {
        let dir = tempfile::tempdir().expect("tmp");
        let cache = Cache::new(dir.path().join("tool"));
        (dir, cache)
    }

    #[test]
    fn an_installed_version_is_resolvable_and_newest_wins() {
        let (_dir, cache) = cache();
        install(&cache, "1.0.0");
        install(&cache, "2.5.0");
        install(&cache, "2.4.0");
        let names: Vec<String> = cache
            .installed()
            .iter()
            .map(|c| c.version.to_dir_name())
            .collect();
        assert_eq!(names, ["2.5.0", "2.4.0", "1.0.0"], "newest first");
        assert_eq!(cache.newest().expect("some").version.to_dir_name(), "2.5.0");
    }

    #[test]
    fn a_version_without_its_marker_is_not_resolvable() {
        let (_dir, cache) = cache();
        let installed = install(&cache, "1.0.0");
        std::fs::remove_file(installed.join(MARKER_FILE)).expect("remove marker");
        assert!(
            cache.installed().is_empty(),
            "a payload with no marker is a torn install, not a version"
        );
    }

    #[test]
    fn a_marker_that_disagrees_with_its_directory_is_ignored() {
        let (_dir, cache) = cache();
        let installed = install(&cache, "1.0.0");
        let wrong = serde_json::to_vec_pretty(&marker("9.9.9")).expect("json");
        std::fs::write(installed.join(MARKER_FILE), wrong).expect("write");
        assert!(
            cache.installed().is_empty(),
            "a moved or hand-edited tree must not be trusted"
        );
    }

    #[test]
    fn an_unknown_marker_version_is_ignored() {
        let (_dir, cache) = cache();
        let installed = install(&cache, "1.0.0");
        let mut future = marker("1.0.0");
        future.marker_version = MARKER_VERSION + 1;
        std::fs::write(
            installed.join(MARKER_FILE),
            serde_json::to_vec_pretty(&future).expect("json"),
        )
        .expect("write");
        assert!(
            cache.installed().is_empty(),
            "a newer format is not guessed at"
        );
    }

    #[test]
    fn a_missing_executable_makes_the_version_unresolvable() {
        let (_dir, cache) = cache();
        let installed = install(&cache, "1.0.0");
        std::fs::remove_file(installed.join(EXE)).expect("remove exe");
        assert!(cache.installed().is_empty());
    }

    #[test]
    fn an_interrupted_install_leaves_nothing_resolvable() {
        let (_dir, cache) = cache();
        install(&cache, "1.0.0");
        // Stage a second version and abandon it, as a killed process would.
        let two = Version::parse("2.0.0").expect("parses");
        let staged = cache.stage(&two).expect("stage");
        std::fs::write(staged.join(EXE), b"half").expect("write");
        assert_eq!(
            cache.installed().len(),
            1,
            "a staged-but-uncommitted version must not be resolvable"
        );
        assert_eq!(cache.newest().expect("some").version.to_dir_name(), "1.0.0");
    }

    #[test]
    fn committing_a_version_that_already_exists_keeps_the_existing_one() {
        let (_dir, cache) = cache();
        let first = install(&cache, "1.0.0");
        let version = Version::parse("1.0.0").expect("parses");
        let staged = cache.stage(&version).expect("stage");
        std::fs::write(staged.join(EXE), b"second").expect("write");
        let second = cache
            .commit(&version, &staged, &marker("1.0.0"))
            .expect("commit is not an error");
        assert_eq!(
            first, second,
            "the same version resolves to the same directory"
        );
        assert_eq!(cache.installed().len(), 1);
        assert!(!staged.exists(), "the losing staged copy is cleaned up");
    }

    #[test]
    fn committing_a_marker_for_a_different_version_is_refused() {
        let (_dir, cache) = cache();
        let version = Version::parse("1.0.0").expect("parses");
        let staged = cache.stage(&version).expect("stage");
        std::fs::write(staged.join(EXE), b"x").expect("write");
        assert!(cache.commit(&version, &staged, &marker("2.0.0")).is_err());
    }

    #[test]
    fn committing_without_the_executable_is_refused() {
        let (_dir, cache) = cache();
        let version = Version::parse("1.0.0").expect("parses");
        let staged = cache.stage(&version).expect("stage");
        assert!(cache.commit(&version, &staged, &marker("1.0.0")).is_err());
    }

    #[test]
    fn sweep_keeps_the_newest_and_never_removes_a_protected_version() {
        let (_dir, cache) = cache();
        for v in ["1.0.0", "2.0.0", "3.0.0", "4.0.0"] {
            install(&cache, v);
        }
        let protected = Version::parse("1.0.0").expect("parses");
        let removed = cache.sweep(2, std::slice::from_ref(&protected));
        let names: Vec<String> = removed.iter().map(Version::to_dir_name).collect();
        assert_eq!(names, ["2.0.0"], "only the unprotected surplus goes");
        let left: Vec<String> = cache
            .installed()
            .iter()
            .map(|c| c.version.to_dir_name())
            .collect();
        assert_eq!(
            left,
            ["4.0.0", "3.0.0", "1.0.0"],
            "kept the two newest plus the protected one"
        );
    }

    #[test]
    fn sweep_removes_abandoned_staging_directories() {
        let (_dir, cache) = cache();
        install(&cache, "1.0.0");
        let two = Version::parse("2.0.0").expect("parses");
        let staged = cache.stage(&two).expect("stage");
        assert!(staged.exists());
        cache.sweep(10, &[]);
        assert!(!staged.exists(), "a killed install must not leak its tree");
    }

    #[test]
    fn a_pin_round_trips_and_clears() {
        let (_dir, cache) = cache();
        std::fs::create_dir_all(cache.root.join(VERSIONS_DIR)).expect("dirs");
        assert!(cache.pin().is_none());
        let version = Version::parse("2.4.0").expect("parses");
        cache.set_pin(&version).expect("set");
        assert_eq!(cache.pin(), Some(version));
        cache.clear_pin().expect("clear");
        assert!(cache.pin().is_none());
        cache
            .clear_pin()
            .expect("clearing an absent pin is not an error");
    }

    #[test]
    fn an_empty_cache_reports_nothing_rather_than_failing() {
        let (_dir, cache) = cache();
        assert!(cache.installed().is_empty());
        assert!(cache.newest().is_none());
        assert!(cache.pin().is_none());
        assert!(cache.sweep(1, &[]).is_empty());
    }
}
