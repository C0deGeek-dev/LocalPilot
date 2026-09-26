//! The git surface: a fixed-subcommand runner, an isolated-worktree wrapper, and
//! a path-containment guard.
//!
//! Security posture (reviewed in the plan's security box):
//! - **Never a shell.** Every git call passes its arguments as an argv array to
//!   `git` directly — there is no shell, no string interpolation of model input,
//!   so an edit path or branch name can never become another command.
//! - **Fixed subcommands only.** The runner is only ever called with the small,
//!   hard-coded set of subcommands this crate needs; nothing here runs a
//!   user/model-supplied command.
//! - **No network.** No `push`, `fetch`, `pull`, or remote subcommand appears
//!   anywhere in this crate.
//! - **Path containment.** [`safe_join`] rejects absolute paths, `..` traversal,
//!   and drive prefixes, so every edit lands strictly inside the worktree.

use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::error::PatchError;

/// Run a fixed git subcommand in `cwd`, returning stdout. Arguments are argv, not
/// a shell string. The first element of `args` is the subcommand.
pub(crate) fn git(cwd: &Path, args: &[&str]) -> Result<String, PatchError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| PatchError::Git {
            args: args.join(" "),
            message: e.to_string(),
        })?;
    if !output.status.success() {
        return Err(PatchError::Git {
            args: args.join(" "),
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `git rev-parse HEAD` in `repo`, trimmed.
pub(crate) fn head_commit(repo: &Path) -> Result<String, PatchError> {
    Ok(git(repo, &["rev-parse", "HEAD"])?.trim().to_string())
}

/// Whether the working tree at `cwd` is clean (no staged or unstaged changes).
pub(crate) fn is_clean(cwd: &Path) -> Result<bool, PatchError> {
    Ok(git(cwd, &["status", "--porcelain"])?.trim().is_empty())
}

/// Project-relative paths (forward-slashed) that differ from `base` in the
/// working tree at `cwd` — the changed-file set, used to enforce scope.
pub(crate) fn changed_paths(cwd: &Path, base: &str) -> Result<Vec<String>, PatchError> {
    let out = git(cwd, &["diff", "--name-only", base])?;
    Ok(out
        .lines()
        .map(|line| line.trim().replace('\\', "/"))
        .filter(|line| !line.is_empty())
        .collect())
}

/// A safe identifier for a branch / worktree directory: ASCII letters, digits,
/// `.`, `_`, `-` only, non-empty, not starting with `-`. Rejects slashes, shell
/// metacharacters, and path traversal outright.
pub(crate) fn validate_branch_name(name: &str) -> Result<(), PatchError> {
    let valid = !name.is_empty()
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if valid {
        Ok(())
    } else {
        Err(PatchError::InvalidBranch(name.to_string()))
    }
}

/// Join a project-relative edit path onto `root`, rejecting any component that
/// could escape the worktree (absolute paths, `..`, root, drive prefix). This is
/// the containment guard: a returned path is guaranteed to be inside `root`.
pub(crate) fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, PatchError> {
    let candidate = Path::new(rel);
    let mut out = root.to_path_buf();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(PatchError::OutsideWorktree(rel.to_string()));
            }
        }
    }
    if !out.starts_with(root) {
        return Err(PatchError::OutsideWorktree(rel.to_string()));
    }
    Ok(out)
}

/// Where every worktree lives, relative to the repository root. In-repo on
/// purpose: a system temporary directory is deep enough on Windows that
/// `git worktree add` fails outright for a repository with long tracked paths.
pub const WORKTREES_DIR: &str = ".localpilot/worktrees";

/// The longest worktree name accepted, so a generated name cannot spend the
/// path budget the repository's own files need.
pub const MAX_WORKTREE_NAME: usize = 40;

/// The longest path Windows accepts when long paths are not enabled
/// (`MAX_PATH`, 260, less the terminating NUL).
const WINDOWS_PATH_LIMIT: usize = 259;

/// An isolated git worktree on its own branch, or detached at an exact
/// revision. All edits land inside it, never in the main working tree.
/// Dropping it (or calling [`Worktree::remove`]) tears it down — the rollback
/// path is to drop it.
#[derive(Debug)]
pub struct Worktree {
    repo_root: PathBuf,
    path: PathBuf,
    branch: String,
    /// Whether `create` made a branch that removal must delete.
    branched: bool,
    removed: bool,
}

impl Worktree {
    /// Create a worktree under `repo_root/.localpilot/worktrees/<branch>` on a new
    /// branch `branch` based on the repo's current `HEAD`.
    pub(crate) fn create(repo_root: &Path, branch: &str) -> Result<Self, PatchError> {
        validate_branch_name(branch)?;
        let dir = repo_root.join(".localpilot").join("worktrees").join(branch);
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent).map_err(|source| PatchError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let dir_str = dir
            .to_str()
            .ok_or_else(|| PatchError::OutsideWorktree(dir.display().to_string()))?;
        git(
            repo_root,
            &["worktree", "add", "-b", branch, dir_str, "HEAD"],
        )?;
        Ok(Self {
            repo_root: repo_root.to_path_buf(),
            path: dir,
            branch: branch.to_string(),
            branched: true,
            removed: false,
        })
    }

    /// Create a worktree named `name` under [`WORKTREES_DIR`], detached at
    /// exactly `revision`, with no branch. Refused before anything is made
    /// when the name is unsafe or longer than [`MAX_WORKTREE_NAME`], or when
    /// the repository's longest tracked path at `revision` would not fit under
    /// the worktree on this platform — that failure names path length, where
    /// git's own would be `Could not reset index file`.
    ///
    /// # Errors
    /// [`PatchError::InvalidBranch`], [`PatchError::PathTooLong`],
    /// [`PatchError::Git`] or [`PatchError::Io`].
    pub fn create_at(repo_root: &Path, name: &str, revision: &str) -> Result<Self, PatchError> {
        validate_branch_name(name)?;
        if name.len() > MAX_WORKTREE_NAME {
            return Err(PatchError::InvalidBranch(format!(
                "{name} (longer than {MAX_WORKTREE_NAME} characters)"
            )));
        }
        let dir = worktrees_root(repo_root).join(name);
        check_path_budget(repo_root, &dir, revision)?;
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent).map_err(|source| PatchError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
            // A worktrees directory that is a link to somewhere else would put
            // the worktree outside the repository: refuse an aliased root.
            let real_parent = std::fs::canonicalize(parent).map_err(|source| PatchError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
            let real_root = std::fs::canonicalize(repo_root).map_err(|source| PatchError::Io {
                path: repo_root.to_path_buf(),
                source,
            })?;
            if !real_parent.starts_with(&real_root) {
                return Err(PatchError::OutsideWorktree(format!(
                    "{} resolves to {}, outside the repository",
                    parent.display(),
                    real_parent.display()
                )));
            }
        }
        let dir_str = dir
            .to_str()
            .ok_or_else(|| PatchError::OutsideWorktree(dir.display().to_string()))?;
        git(
            repo_root,
            &["worktree", "add", "--detach", dir_str, revision],
        )?;
        Ok(Self {
            repo_root: repo_root.to_path_buf(),
            path: dir,
            branch: name.to_string(),
            branched: false,
            removed: false,
        })
    }

    /// Reattach to an existing worktree previously created by [`Worktree::create`],
    /// without running `git worktree add`. Used by a later process (e.g. a separate
    /// `promote`/`discard` CLI invocation) to act on a proposal it did not create.
    /// Returns [`PatchError::UnknownProposal`] if no worktree directory is present.
    pub(crate) fn open(repo_root: &Path, branch: &str) -> Result<Self, PatchError> {
        validate_branch_name(branch)?;
        let dir = repo_root.join(".localpilot").join("worktrees").join(branch);
        if !dir.is_dir() {
            return Err(PatchError::UnknownProposal(branch.to_string()));
        }
        Ok(Self {
            repo_root: repo_root.to_path_buf(),
            path: dir,
            branch: branch.to_string(),
            branched: true,
            removed: false,
        })
    }

    /// The worktree's directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn branch(&self) -> &str {
        &self.branch
    }

    /// Suppress drop-time removal so the worktree survives this process for a later
    /// [`Worktree::open`] (e.g. a separate promote/discard invocation). The worktree
    /// is then only removed by an explicit [`Worktree::remove`] on a reattached handle.
    pub(crate) fn detach(&mut self) {
        self.removed = true;
    }

    /// Remove the worktree and delete its branch, if it has one — the
    /// rollback. Best-effort but surfaces the first git error.
    ///
    /// # Errors
    /// [`PatchError::Git`] when git cannot remove the worktree.
    pub fn remove(&mut self) -> Result<(), PatchError> {
        if self.removed {
            return Ok(());
        }
        self.removed = true;
        let path_str = self.path.to_string_lossy().to_string();
        git(
            &self.repo_root,
            &["worktree", "remove", "--force", &path_str],
        )?;
        if self.branched {
            // Branch deletion is best-effort: the worktree is already gone.
            let _ = git(&self.repo_root, &["branch", "-D", &self.branch]);
        }
        Ok(())
    }
}

/// The directory [`WORKTREES_DIR`] names under `repo_root`, joined component by
/// component so the path uses one separator throughout.
#[must_use]
pub fn worktrees_root(repo_root: &Path) -> PathBuf {
    WORKTREES_DIR
        .split('/')
        .fold(repo_root.to_path_buf(), |path, part| path.join(part))
}

/// Refuse a worktree directory too deep for the repository's longest tracked
/// path at `revision`. Only Windows without `core.longpaths` has a limit this
/// can hit; elsewhere it always passes.
///
/// # Errors
/// [`PatchError::PathTooLong`] naming the lengths, or [`PatchError::Git`] when
/// the revision's tree cannot be listed.
pub fn check_path_budget(repo_root: &Path, dir: &Path, revision: &str) -> Result<(), PatchError> {
    if !cfg!(windows) {
        return Ok(());
    }
    let long_paths = git(repo_root, &["config", "--get", "core.longpaths"])
        .map(|value| value.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if long_paths {
        return Ok(());
    }
    let listing = git(repo_root, &["ls-tree", "-r", "--name-only", revision])?;
    let longest = listing.lines().map(str::len).max().unwrap_or(0);
    let dir_chars = dir.as_os_str().len();
    let total = dir_chars + 1 + longest;
    if total > WINDOWS_PATH_LIMIT {
        return Err(PatchError::PathTooLong {
            dir: dir.to_path_buf(),
            dir_chars,
            longest_tracked: longest,
            total,
            limit: WINDOWS_PATH_LIMIT,
        });
    }
    Ok(())
}

/// Remove every worktree under [`WORKTREES_DIR`] whose name starts with
/// `prefix` — what a process that was killed mid-run left behind, since a
/// killed process runs no cleanup. Returns each directory found and whether
/// it was removed. Only names beginning with `prefix` are touched.
#[must_use]
pub fn sweep_worktrees(repo_root: &Path, prefix: &str) -> Vec<(PathBuf, Result<(), PatchError>)> {
    let base = worktrees_root(repo_root);
    let Ok(entries) = std::fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| !prefix.is_empty() && name.starts_with(prefix))
        })
        .collect();
    found.sort();
    let swept = found
        .into_iter()
        .map(|path| {
            let path_str = path.to_string_lossy().to_string();
            let removed = git(repo_root, &["worktree", "remove", "--force", &path_str])
                .map(|_| ())
                .or_else(|error| {
                    // Not a registered worktree any more (its metadata was
                    // pruned); the directory is plain leftovers.
                    std::fs::remove_dir_all(&path).map_err(|_| error)
                });
            (path, removed)
        })
        .collect();
    let _ = git(repo_root, &["worktree", "prune"]);
    swept
}

impl Drop for Worktree {
    fn drop(&mut self) {
        if !self.removed {
            let _ = self.remove();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_rejects_escapes_and_accepts_normal_paths() {
        let root = Path::new("/repo/wt");
        assert!(safe_join(root, "src/a.rs").is_ok());
        assert!(safe_join(root, "./src/a.rs").is_ok());
        assert!(safe_join(root, "../escape.rs").is_err());
        assert!(safe_join(root, "a/../../escape.rs").is_err());
        #[cfg(windows)]
        assert!(safe_join(root, "C:\\windows\\system32").is_err());
        #[cfg(not(windows))]
        assert!(safe_join(root, "/etc/passwd").is_err());
    }

    #[test]
    fn branch_names_are_validated() {
        assert!(validate_branch_name("self-review-1").is_ok());
        assert!(validate_branch_name("fix.todo_42").is_ok());
        assert!(validate_branch_name("").is_err());
        assert!(validate_branch_name("-evil").is_err());
        assert!(validate_branch_name("a/b").is_err());
        assert!(validate_branch_name("a;rm -rf").is_err());
        assert!(validate_branch_name("a..b").is_ok()); // dots ok, but no path semantics in argv
    }
}
