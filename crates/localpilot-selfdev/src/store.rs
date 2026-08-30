//! The immutable version store: every self-dev build in its own directory,
//! never overwritten, never mutated once written.
//!
//! This is the load-bearing invariant of the whole reload story. A running
//! process is `exec`'d from some path; if a later build overwrites that path,
//! the running binary becomes `(deleted)` and the process is stranded. So a
//! build lands in `versions/<label>/`, a directory named by the source label,
//! and **nothing ever writes into an existing one**. A rebuild of the same
//! source is a no-op; a rebuild of different source is a different directory.
//! Switching which one runs is a job for the channel pointer (see
//! [`crate::channel`]), never a copy over a live file.
//!
//! Installs go in by **copy**, deliberately, even though a hard link would be
//! free. The source is a live cargo build output, and cargo owns that path: a
//! later rebuild rewrites it, in place on some platforms. A hard link would then
//! share an inode with that path, so the next build would silently mutate a
//! stored version a process might be running from — the exact thing this store
//! exists to prevent. A copy severs that tie; immutability is worth one file
//! copy.

use std::path::{Path, PathBuf};

use crate::error::SelfDevError;
use crate::marker::{BuildMarker, BUILD_MARKER_VERSION, MARKER_FILE};

/// Directory holding the per-label installs.
const VERSIONS_DIR: &str = "versions";

/// One stored version the channel pointers may resolve to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredVersion {
    /// The source label — also the directory name.
    pub label: String,
    /// Its marker.
    pub marker: BuildMarker,
    /// The directory it lives in.
    pub dir: PathBuf,
}

impl StoredVersion {
    /// Full path to this version's executable.
    #[must_use]
    pub fn executable(&self) -> PathBuf {
        self.dir.join(&self.marker.executable)
    }
}

/// The immutable version store rooted at `root`.
#[derive(Debug, Clone)]
pub struct VersionStore {
    root: PathBuf,
}

impl VersionStore {
    /// A store rooted at `root` (the self-dev subtree).
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The store's root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory a given label installs into.
    #[must_use]
    pub fn version_dir(&self, label: &str) -> PathBuf {
        self.root.join(VERSIONS_DIR).join(label)
    }

    /// Look up one stored version by label.
    #[must_use]
    pub fn get(&self, label: &str) -> Option<StoredVersion> {
        let dir = self.version_dir(label);
        let marker = read_marker(&dir)?;
        // The marker must agree with where it was found and name a file that is
        // actually there; a mismatch means a hand-edited or torn tree, and
        // guessing is worse than ignoring it.
        if marker.marker_version != BUILD_MARKER_VERSION || marker.label != label {
            return None;
        }
        if !dir.join(&marker.executable).is_file() {
            return None;
        }
        Some(StoredVersion {
            label: label.to_string(),
            marker,
            dir,
        })
    }

    /// Every stored version carrying a coherent marker, in directory order.
    ///
    /// A directory whose marker is missing, unreadable, of an unknown format,
    /// or inconsistent with its own name is skipped — a torn install must not
    /// become resolvable just because it exists on disk.
    #[must_use]
    pub fn installed(&self) -> Vec<StoredVersion> {
        let Ok(entries) = std::fs::read_dir(self.root.join(VERSIONS_DIR)) else {
            return Vec::new();
        };
        let mut found: Vec<StoredVersion> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_str()?.to_string();
                self.get(&name)
            })
            .collect();
        found.sort_by(|a, b| a.label.cmp(&b.label));
        found
    }

    /// Install `executable` as the stored version `label`, recorded by `marker`.
    ///
    /// Idempotent and immutable: if `label` already exists with a coherent
    /// marker, the existing install is returned untouched and `executable` is not
    /// read — a rebuild of the same source must never disturb a version a process
    /// might be running from. Otherwise the payload is copied into a staging
    /// directory, the marker is written **last**, and the whole directory is
    /// renamed into place: before the rename the version does not exist, after it
    /// the version is complete.
    ///
    /// # Errors
    /// Returns [`SelfDevError::Invalid`] when the marker disagrees with `label`,
    /// and [`SelfDevError::Io`] when staging, copying, or the commit fails.
    pub fn install(
        &self,
        label: &str,
        executable: &Path,
        marker: &BuildMarker,
    ) -> Result<StoredVersion, SelfDevError> {
        if marker.label != label {
            return Err(SelfDevError::Invalid(format!(
                "marker names label {:?} but is being installed as {:?}",
                marker.label, label
            )));
        }
        if let Some(existing) = self.get(label) {
            return Ok(existing);
        }

        let versions = self.root.join(VERSIONS_DIR);
        std::fs::create_dir_all(&versions).map_err(SelfDevError::io)?;
        let staging = versions.join(format!(".staging-{label}-{}", std::process::id()));
        // A leftover staging tree from a killed run is removed, not reused.
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging).map_err(SelfDevError::io)?;

        let staged_exe = staging.join(&marker.executable);
        std::fs::copy(executable, &staged_exe)
            .map_err(SelfDevError::io)
            .inspect_err(|_| {
                let _ = std::fs::remove_dir_all(&staging);
            })?;

        let body = serde_json::to_vec_pretty(marker).map_err(SelfDevError::io)?;
        std::fs::write(staging.join(MARKER_FILE), body)
            .map_err(SelfDevError::io)
            .inspect_err(|_| {
                let _ = std::fs::remove_dir_all(&staging);
            })?;

        let target = self.version_dir(label);
        match std::fs::rename(&staging, &target) {
            Ok(()) => {}
            // Someone else won the race; theirs is as good as ours.
            Err(_) if target.is_dir() => {
                let _ = std::fs::remove_dir_all(&staging);
            }
            Err(error) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(SelfDevError::io(error));
            }
        }
        self.get(label).ok_or_else(|| {
            SelfDevError::Io("the install committed but did not become resolvable".to_string())
        })
    }

    /// Reclaim disk by removing stored versions beyond the `keep` most recent,
    /// never touching a label in `protected` (a channel points at it, or a
    /// process is running from it).
    ///
    /// A copy-in immutable store grows by one whole binary per distinct build; a
    /// self-dev loop that rebuilds often would otherwise fill the disk. Recency is
    /// by directory modification time — the moment the version was committed into
    /// place — so the freshest builds are kept. A protected label is kept
    /// regardless of age or the `keep` budget; that is the whole point of
    /// protecting it. Leftover staging directories from a killed install are swept
    /// too.
    ///
    /// Returns the labels actually removed, so a caller can report the sweep
    /// rather than silently reclaiming disk. A directory that will not delete
    /// (still in use) is skipped, not fatal.
    pub fn sweep(&self, keep: usize, protected: &[String]) -> Vec<String> {
        let mut versions = self.installed();
        // Newest first by commit-time (mtime). An unreadable mtime sorts oldest,
        // so a torn or foreign directory is a sweep candidate rather than a
        // protected one.
        versions.sort_by(|a, b| dir_mtime(&b.dir).cmp(&dir_mtime(&a.dir)));

        let mut removed = Vec::new();
        for (index, version) in versions.iter().enumerate() {
            if protected.contains(&version.label) || index < keep {
                continue;
            }
            if std::fs::remove_dir_all(&version.dir).is_ok() {
                removed.push(version.label.clone());
            }
        }

        // A killed install leaves a `.staging-*` tree nothing ever reads.
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
}

/// Read and parse the marker in `dir`, if any.
fn read_marker(dir: &Path) -> Option<BuildMarker> {
    let body = std::fs::read_to_string(dir.join(MARKER_FILE)).ok()?;
    serde_json::from_str(&body).ok()
}

/// A directory's modification time, for recency ordering. `None` (unreadable)
/// sorts as oldest.
fn dir_mtime(dir: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(dir).and_then(|meta| meta.modified()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marker(label: &str) -> BuildMarker {
        BuildMarker::new(
            label,
            "abc1234",
            "fp",
            true,
            format!("2.6.0-selfdev-{label}"),
            executable_name(),
        )
    }

    fn executable_name() -> String {
        localpilot_dist::executable_name(crate::builder::TOOL)
    }

    fn write_exe(dir: &Path, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("dir");
        let path = dir.join("built");
        std::fs::write(&path, body).expect("write");
        path
    }

    #[test]
    fn two_distinct_labels_coexist_and_the_store_owns_its_own_bytes() {
        let temp = tempfile::tempdir().expect("temp");
        let store = VersionStore::new(temp.path());
        // Both builds land at the *same* source path, the way cargo reuses its
        // output path. If the store hard-linked instead of copying, rewriting
        // this path for the second build would mutate the first stored version —
        // the running-binary hazard this store exists to prevent.
        let built = temp.path().join("build");

        let one = write_exe(&built, "one");
        store.install("aaaa", &one, &marker("aaaa")).expect("one");
        let two = write_exe(&built, "two"); // rewrites the same path in place
        store.install("bbbb", &two, &marker("bbbb")).expect("two");

        let labels: Vec<_> = store.installed().into_iter().map(|v| v.label).collect();
        assert_eq!(labels, vec!["aaaa".to_string(), "bbbb".to_string()]);
        assert_eq!(
            std::fs::read_to_string(store.get("aaaa").unwrap().executable()).unwrap(),
            "one",
            "rewriting the build-output path must not reach into a stored version"
        );
        assert_eq!(
            std::fs::read_to_string(store.get("bbbb").unwrap().executable()).unwrap(),
            "two"
        );
    }

    #[test]
    fn reinstalling_a_label_is_an_untouched_no_op() {
        let temp = tempfile::tempdir().expect("temp");
        let store = VersionStore::new(temp.path());
        let built = temp.path().join("build");

        let first = write_exe(&built, "original");
        let stored = store
            .install("aaaa", &first, &marker("aaaa"))
            .expect("first");
        let original_bytes = std::fs::read_to_string(stored.executable()).unwrap();

        // A second install of the same label with *different* bytes must not
        // change what is stored: a process may be running from it.
        let second = write_exe(&built, "different");
        let again = store
            .install("aaaa", &second, &marker("aaaa"))
            .expect("again");

        assert_eq!(again.dir, stored.dir);
        assert_eq!(
            std::fs::read_to_string(again.executable()).unwrap(),
            original_bytes,
            "an immutable version must never be rewritten by a re-install"
        );
    }

    #[test]
    fn a_torn_install_missing_its_marker_is_not_resolvable() {
        let temp = tempfile::tempdir().expect("temp");
        let store = VersionStore::new(temp.path());
        // A directory that exists but carries no marker (a crash mid-install,
        // simulated) must be invisible to resolution.
        std::fs::create_dir_all(store.version_dir("cccc")).expect("dir");
        assert!(store.get("cccc").is_none());
        assert!(store.installed().is_empty());
    }

    #[test]
    fn a_marker_disagreeing_with_its_directory_is_rejected() {
        let temp = tempfile::tempdir().expect("temp");
        let store = VersionStore::new(temp.path());
        let built = temp.path().join("build");
        let exe = write_exe(&built, "x");

        let err = store
            .install("aaaa", &exe, &marker("bbbb"))
            .expect_err("a mismatched marker must be refused");
        assert!(matches!(err, SelfDevError::Invalid(_)));
    }

    #[test]
    fn the_stored_executable_carries_the_markers_name() {
        let temp = tempfile::tempdir().expect("temp");
        let store = VersionStore::new(temp.path());
        let built = temp.path().join("build");
        let exe = write_exe(&built, "x");

        let stored = store
            .install("aaaa", &exe, &marker("aaaa"))
            .expect("install");
        assert!(stored.executable().ends_with(executable_name()));
        assert!(stored.executable().is_file());
    }

    /// Install `count` distinct labels `v0..v{count}`.
    fn install_many(store: &VersionStore, built: &Path, count: usize) {
        for i in 0..count {
            let label = format!("v{i}");
            let exe = write_exe(built, &label);
            store
                .install(&label, &exe, &marker(&label))
                .expect("install");
        }
    }

    #[test]
    fn sweep_keeps_everything_when_keep_exceeds_the_count() {
        let temp = tempfile::tempdir().expect("temp");
        let store = VersionStore::new(temp.path());
        install_many(&store, &temp.path().join("build"), 2);

        let removed = store.sweep(10, &[]);
        assert!(removed.is_empty());
        assert_eq!(store.installed().len(), 2);
    }

    #[test]
    fn sweep_always_keeps_a_protected_label_even_at_keep_zero() {
        let temp = tempfile::tempdir().expect("temp");
        let store = VersionStore::new(temp.path());
        install_many(&store, &temp.path().join("build"), 3);

        // keep=0 means recency does not matter; only the protected survives.
        let removed = store.sweep(0, &["v1".to_string()]);
        let survivors: Vec<_> = store.installed().into_iter().map(|v| v.label).collect();
        assert_eq!(
            survivors,
            vec!["v1".to_string()],
            "only the protected label remains"
        );
        assert_eq!(removed.len(), 2);
        assert!(!removed.contains(&"v1".to_string()));
    }

    #[test]
    fn sweep_removes_down_to_the_keep_count() {
        let temp = tempfile::tempdir().expect("temp");
        let store = VersionStore::new(temp.path());
        install_many(&store, &temp.path().join("build"), 3);

        let removed = store.sweep(1, &[]);
        assert_eq!(removed.len(), 2, "three installed, keep one, remove two");
        assert_eq!(store.installed().len(), 1);
    }

    #[test]
    fn sweep_reclaims_leftover_staging_directories() {
        let temp = tempfile::tempdir().expect("temp");
        let store = VersionStore::new(temp.path());
        let versions = temp.path().join("versions");
        std::fs::create_dir_all(&versions).expect("versions dir");
        // Simulate a killed install.
        let staging = versions.join(".staging-vX-123");
        std::fs::create_dir_all(&staging).expect("staging");

        store.sweep(10, &[]);
        assert!(
            !staging.exists(),
            "a leftover staging tree must be reclaimed"
        );
    }
}
