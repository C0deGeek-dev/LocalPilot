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

/// An isolated git worktree on its own branch. Created off `HEAD`; all edits land
/// inside it, never in the main working tree. Dropping it (or calling
/// [`Worktree::remove`]) tears it down — the rollback path is to drop it.
#[derive(Debug)]
pub(crate) struct Worktree {
    repo_root: PathBuf,
    path: PathBuf,
    branch: String,
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
            removed: false,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn branch(&self) -> &str {
        &self.branch
    }

    /// Remove the worktree and delete its branch — the rollback. Best-effort but
    /// surfaces the first git error.
    pub(crate) fn remove(&mut self) -> Result<(), PatchError> {
        if self.removed {
            return Ok(());
        }
        self.removed = true;
        let path_str = self.path.to_string_lossy().to_string();
        git(
            &self.repo_root,
            &["worktree", "remove", "--force", &path_str],
        )?;
        // Branch deletion is best-effort: the worktree is already gone.
        let _ = git(&self.repo_root, &["branch", "-D", &self.branch]);
        Ok(())
    }
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
