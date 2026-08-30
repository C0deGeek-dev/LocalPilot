//! Fetching a repository snapshot — the one network seam of skill-source
//! management (LocalHub#40).
//!
//! [`RepoFetcher`] is the injectable boundary: production uses [`GitFetcher`],
//! which shells out to the system `git` to shallow-clone a public HTTPS repository
//! on its default branch; tests inject a fake that materializes a fixture tree, so
//! the whole management surface is covered without any live-network access.
//!
//! Fetching only ever *reads* a remote and writes a working tree into a
//! caller-controlled staging directory. No script in the fetched tree is run, and
//! the git invocation is argv-only (never a shell) with interactive credential
//! prompts disabled, so a private or credential-guarded URL fails fast rather than
//! blocking.

use std::path::Path;
use std::process::Command;

use crate::error::SkillError;
use crate::source::normalize_url;

/// A generous ceiling on a fetched snapshot's total size and file count. A
/// repository larger than this is refused rather than cached — skill catalogs are
/// small, and an unbounded tree is a denial-of-service risk, not a real source.
pub const MAX_SNAPSHOT_BYTES: u64 = 64 * 1024 * 1024;
/// The matching file-count ceiling for a fetched snapshot.
pub const MAX_SNAPSHOT_FILES: usize = 20_000;

/// The validated result of a fetch: the commit the snapshot was taken at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub commit: String,
}

/// Fetch a public HTTPS Git repository's default branch into a staging directory.
///
/// Implementations must treat the input as untrusted: only public HTTPS URLs on
/// the default branch, no credentials, and no execution of fetched content.
pub trait RepoFetcher {
    /// Fetch `url` into `dest` (an empty or not-yet-existing directory the caller
    /// owns) and return the snapshot commit.
    ///
    /// # Errors
    /// Returns [`SkillError::Rejected`] if the URL is not an acceptable public
    /// HTTPS URL, or [`SkillError::Fetch`] if the network operation fails.
    fn fetch(&self, url: &str, dest: &Path) -> Result<Snapshot, SkillError>;
}

/// The production fetcher: a shallow, single-branch `git clone` of a public HTTPS
/// repository, argv-only and with credential prompts disabled.
#[derive(Debug, Default, Clone, Copy)]
pub struct GitFetcher;

impl RepoFetcher for GitFetcher {
    fn fetch(&self, url: &str, dest: &Path) -> Result<Snapshot, SkillError> {
        // Defense in depth: normalize again here, so the fetcher never spawns git
        // on a non-HTTPS or credential-bearing URL even if a caller skipped it.
        let url = normalize_url(url)?;
        let dest_str = dest.to_str().ok_or_else(|| {
            SkillError::Rejected("destination path is not valid UTF-8".to_string())
        })?;

        // Shallow, single-branch, default-branch clone. `--` terminates option
        // parsing so a hostile URL can never be read as a flag.
        run_git(&[
            "clone",
            "--depth",
            "1",
            "--single-branch",
            "--no-tags",
            "--",
            &url,
            dest_str,
        ])?;

        let commit = run_git(&["-C", dest_str, "rev-parse", "HEAD"])?
            .trim()
            .to_string();
        if commit.is_empty() {
            return Err(SkillError::Fetch(
                "clone produced no commit (empty repository?)".to_string(),
            ));
        }
        Ok(Snapshot { commit })
    }
}

/// Run `git` with a fixed argument vector, credential prompts disabled, returning
/// its stdout. Never a shell; the process inherits no interactive terminal.
fn run_git(args: &[&str]) -> Result<String, SkillError> {
    let output = Command::new("git")
        .args(args)
        // Fail fast instead of blocking on a credential prompt for a private or
        // otherwise inaccessible repository.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .output()
        .map_err(|e| SkillError::Fetch(format!("could not run git: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SkillError::Fetch(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or_default(),
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Ensure a fetched tree stays within [`MAX_SNAPSHOT_BYTES`]/[`MAX_SNAPSHOT_FILES`].
/// Walks the tree, refuses a symlink that escapes it, and refuses once either
/// ceiling is crossed, so an unbounded or symlink-hostile snapshot is never
/// accepted into the cache.
///
/// # Errors
/// Returns [`SkillError::Rejected`] if a bound is exceeded or an escaping symlink
/// is found, or [`SkillError::Io`] on a read failure.
pub fn ensure_snapshot_within_bounds(root: &Path) -> Result<(), SkillError> {
    let mut budget = TreeBudget {
        max_bytes: MAX_SNAPSHOT_BYTES,
        max_files: MAX_SNAPSHOT_FILES,
        bytes: 0,
        files: 0,
    };
    walk_bounded(root, root, &mut budget)
}

/// A running budget for a bounded tree walk.
pub(crate) struct TreeBudget {
    pub(crate) max_bytes: u64,
    pub(crate) max_files: usize,
    pub(crate) bytes: u64,
    pub(crate) files: usize,
}

/// Walk `dir` (rooted at `root` for containment checks), charging each regular
/// file against `budget` and refusing an escaping symlink or a crossed ceiling.
pub(crate) fn walk_bounded(
    root: &Path,
    dir: &Path,
    budget: &mut TreeBudget,
) -> Result<(), SkillError> {
    let entries = std::fs::read_dir(dir).map_err(|source| SkillError::Io {
        path: dir.display().to_string(),
        source,
    })?;
    for entry in entries.flatten() {
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path).map_err(|source| SkillError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let file_type = meta.file_type();
        if file_type.is_symlink() {
            // A symlink is only allowed if its target stays inside the tree.
            let target = std::fs::canonicalize(&path).map_err(|_| {
                SkillError::Rejected(format!("dangling symlink at {}", path.display()))
            })?;
            let root_canon = std::fs::canonicalize(root).map_err(|source| SkillError::Io {
                path: root.display().to_string(),
                source,
            })?;
            if !target.starts_with(&root_canon) {
                return Err(SkillError::Rejected(format!(
                    "symlink escapes the repository: {}",
                    path.display()
                )));
            }
            continue;
        }
        if file_type.is_dir() {
            walk_bounded(root, &path, budget)?;
        } else {
            budget.files += 1;
            budget.bytes = budget.bytes.saturating_add(meta.len());
            if budget.files > budget.max_files {
                return Err(SkillError::Rejected(format!(
                    "repository has too many files (> {})",
                    budget.max_files
                )));
            }
            if budget.bytes > budget.max_bytes {
                return Err(SkillError::Rejected(format!(
                    "repository is too large (> {} bytes)",
                    budget.max_bytes
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn git_fetcher_rejects_non_https_before_spawning() {
        let dir = tempfile::tempdir().unwrap();
        let err = GitFetcher
            .fetch("git@github.com:o/r.git", &dir.path().join("clone"))
            .unwrap_err();
        assert!(matches!(err, SkillError::Rejected(_)), "got {err:?}");
    }

    #[test]
    fn bounds_accept_a_small_tree_and_reject_an_over_count_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("b.txt"), "world").unwrap();
        // A small tree is within bounds.
        ensure_snapshot_within_bounds(dir.path()).unwrap();

        // A tight budget crosses the file ceiling.
        let mut budget = TreeBudget {
            max_bytes: MAX_SNAPSHOT_BYTES,
            max_files: 1,
            bytes: 0,
            files: 0,
        };
        assert!(matches!(
            walk_bounded(dir.path(), dir.path(), &mut budget),
            Err(SkillError::Rejected(_))
        ));
    }
}
