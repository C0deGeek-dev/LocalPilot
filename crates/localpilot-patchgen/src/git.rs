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

use std::io::{Seek, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::PatchError;

/// Run a fixed git subcommand in `cwd`, returning stdout. Arguments are argv, not
/// a shell string. The first element of `args` is the subcommand.
pub(crate) fn git(cwd: &Path, args: &[&str]) -> Result<String, PatchError> {
    // Detect configured executable filters before checkout/index/diff operations,
    // not after one of those operations has already run the helper.
    let committed_diff = args.first() == Some(&"diff")
        && args.len() >= 3
        && args[args.len() - 2..].iter().all(|arg| {
            *arg == "HEAD"
                || (matches!(arg.len(), 40 | 64) && arg.bytes().all(|b| b.is_ascii_hexdigit()))
        });
    if (!committed_diff
        && matches!(
            args.first(),
            Some(&"add" | &"commit" | &"status" | &"diff" | &"merge")
        ))
        || args.starts_with(&["worktree", "add"])
    {
        reject_executable_filters(cwd, args)?;
    }
    let mut command = static_git(cwd)?;
    command.args(args);
    let output = command.output().map_err(|e| PatchError::Git {
        args: args.join(" "),
        message: e.to_string(),
    })?;
    if !output.status.success() {
        return Err(PatchError::Git {
            args: args.join(" "),
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    String::from_utf8(output.stdout)
        .map_err(|_| PatchError::UnsupportedGitPolicy("Git output is not UTF-8".into()))
}

fn static_git(cwd: &Path) -> Result<Command, PatchError> {
    let mut roots =
        vec![std::fs::canonicalize(cwd).map_err(|error| policy_error(&error.to_string()))?];
    for ancestor in cwd.ancestors().filter(|path| path.join(".git").exists()) {
        let root =
            std::fs::canonicalize(ancestor).map_err(|error| policy_error(&error.to_string()))?;
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    let refs: Vec<_> = roots.iter().map(PathBuf::as_path).collect();
    let program = crate::validation::resolve_host_program("git", &refs).map_err(|_| {
        policy_error("a trusted host Git executable outside the project is required")
    })?;
    let mut search = vec![program
        .parent()
        .ok_or_else(|| policy_error("host Git has no parent"))?
        .to_path_buf()];
    if let Some(path) = std::env::var_os("PATH") {
        search.extend(
            std::env::split_paths(&path)
                .filter_map(|dir| crate::validation::trusted_search_directory(&dir, &refs)),
        );
    }
    let path = std::env::join_paths(search)
        .map_err(|_| policy_error("unsupported host Git search path"))?;
    let mut command = Command::new(program);
    command
        .env("PATH", path)
        .env_remove("GIT_EXEC_PATH")
        .env("NoDefaultCurrentDirectoryInExePath", "1")
        .args([
            "--literal-pathspecs",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "tag.gpgSign=false",
            "-c",
            "protocol.allow=never",
            "-c",
            "submodule.recurse=false",
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
        ])
        .env_remove("GIT_EXTERNAL_DIFF")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_ATTR_SOURCE")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .current_dir(cwd);
    Ok(command)
}

fn reject_executable_filters(cwd: &Path, operation: &[&str]) -> Result<(), PatchError> {
    // Names only: configured helper command values may contain secrets. Named
    // hooks can exist independently of traditional core.hooksPath on newer Git.
    let named = static_git(cwd)?
        .args(["config", "--name-only", "--get-regexp", "^hook\\."])
        .output()
        .map_err(|error| policy_error(&error.to_string()))?;
    match named.status.code() {
        Some(1) => {}
        Some(0) => {
            return Err(policy_error(
                "configured named hooks require separate review",
            ))
        }
        _ => return Err(policy_error("cannot inspect named-hook configuration")),
    }
    // Unused configured filters (notably global Git LFS) are harmless. Inspect
    // actual attributes instead of forbidding their mere presence in config.
    let paths = static_output(
        cwd,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    inspect_filter_attributes(cwd, &paths, None)?;
    inspect_filter_attributes(cwd, &paths, Some("--cached"))?;

    if operation.first() == Some(&"merge") || operation.starts_with(&["worktree", "add"]) {
        inspect_tree_filters(cwd, "HEAD")?;
        let source = operation
            .last()
            .ok_or_else(|| policy_error("missing checkout source"))?;
        if *source != "HEAD" {
            inspect_tree_filters(cwd, source)?;
        }
    }
    Ok(())
}

fn inspect_tree_filters(cwd: &Path, source: &str) -> Result<(), PatchError> {
    if source.starts_with('-') {
        return Err(policy_error("invalid checkout source"));
    }
    let expression = format!("{source}^{{tree}}");
    let tree = static_output(cwd, &["rev-parse", "--verify", &expression])?;
    let tree = std::str::from_utf8(&tree)
        .map_err(|_| policy_error("invalid Git tree identity"))?
        .trim();
    if !matches!(tree.len(), 40 | 64) || !tree.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(policy_error("invalid Git tree identity"));
    }
    let paths = static_output(cwd, &["ls-tree", "-r", "--name-only", "-z", tree])?;
    inspect_filter_attributes(cwd, &paths, Some(&format!("--source={tree}")))
}

fn inspect_filter_attributes(
    cwd: &Path,
    paths: &[u8],
    source: Option<&str>,
) -> Result<(), PatchError> {
    let output = attribute_output(cwd, paths, source)?;
    let mut fields = output.split(|byte| *byte == 0);
    loop {
        match fields.next() {
            None | Some([]) => break,
            Some(_) => {}
        }
        let attribute = fields
            .next()
            .ok_or_else(|| policy_error("incomplete attribute proof"))?;
        let _value = fields
            .next()
            .ok_or_else(|| policy_error("incomplete attribute proof"))?;
        // --all omits truly unspecified attributes. Drivers can literally be
        // named "unspecified" or "unset"; requesting only filter would make
        // those values indistinguishable from sentinels. Refuse every explicit
        // filter attribute, including -filter, in the supported static path.
        if attribute == b"filter" {
            return Err(policy_error("checkout/index filter attributes require separate review; refusing static proposal operation"));
        }
    }
    Ok(())
}

pub(crate) fn attribute_output(
    cwd: &Path,
    paths: &[u8],
    source: Option<&str>,
) -> Result<Vec<u8>, PatchError> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    // File-backed stdin avoids the documented check-attr pipe-buffer deadlock.
    let mut input = tempfile::tempfile().map_err(|error| policy_error(&error.to_string()))?;
    input
        .write_all(paths)
        .and_then(|()| input.rewind())
        .map_err(|error| policy_error(&error.to_string()))?;
    let mut command = static_git(cwd)?;
    command.arg("check-attr");
    if let Some(source) = source {
        command.arg(source);
    }
    let output = command
        .args(["-z", "--stdin", "--all"])
        .stdin(Stdio::from(input))
        .output()
        .map_err(|error| policy_error(&error.to_string()))?;
    if !output.status.success() {
        return Err(policy_error("Git cannot establish checkout attributes; a Git with check-attr --source support is required"));
    }
    Ok(output.stdout)
}

pub(crate) fn config_value(cwd: &Path, name: &str) -> Result<Option<String>, PatchError> {
    let output = static_git(cwd)?
        .args(["config", "--get", name])
        .output()
        .map_err(|error| policy_error(&error.to_string()))?;
    match output.status.code() {
        Some(0) => String::from_utf8(output.stdout)
            .map(Some)
            .map_err(|_| policy_error("non-UTF8 text policy")),
        Some(1) => Ok(None),
        _ => Err(policy_error("cannot inspect text configuration")),
    }
}

fn static_output(cwd: &Path, args: &[&str]) -> Result<Vec<u8>, PatchError> {
    let output = static_git(cwd)?
        .args(args)
        .output()
        .map_err(|error| policy_error(&error.to_string()))?;
    if !output.status.success() {
        return Err(policy_error("cannot inspect static Git metadata"));
    }
    Ok(output.stdout)
}

fn policy_error(message: &str) -> PatchError {
    PatchError::UnsupportedGitPolicy(message.into())
}

/// Reject existing symlinks/junctions before a scoped proposal write. Lexical
/// safe_join alone cannot prevent following a symlink committed in the base.
pub(crate) fn contained_edit_path(root: &Path, rel: &str) -> Result<PathBuf, PatchError> {
    let target = safe_join(root, rel)?;
    let mut current = root.to_path_buf();
    for component in Path::new(rel).components() {
        if let Component::Normal(part) = component {
            current.push(part);
            match std::fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(PatchError::OutsideWorktree(rel.into()));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(PatchError::Io {
                        path: current,
                        source,
                    })
                }
            }
        }
    }
    Ok(target)
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
    let out = git(
        cwd,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--name-only",
            base,
        ],
    )?;
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
/// Newly created temporary worktrees roll back on drop. Reopened persisted
/// worktrees remain inspectable until explicit [`Worktree::remove`].
#[derive(Debug)]
pub struct Worktree {
    repo_root: PathBuf,
    path: PathBuf,
    branch: String,
    /// Whether `create` made a branch that removal must delete.
    branched: bool,
    removed: bool,
    remove_on_drop: bool,
}

impl Worktree {
    /// Create a worktree under `repo_root/.localpilot/worktrees/<branch>` on a new
    /// branch `branch` based on the repo's current `HEAD`.
    pub(crate) fn create(repo_root: &Path, branch: &str) -> Result<Self, PatchError> {
        validate_branch_name(branch)?;
        let dir = contained_edit_path(repo_root, &format!("{WORKTREES_DIR}/{branch}"))?;
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
            remove_on_drop: true,
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
            remove_on_drop: true,
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
            remove_on_drop: false,
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
        self.remove_on_drop = false;
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
    if !base.is_dir() {
        return Vec::new();
    }
    // A worktrees directory that is a link to somewhere else is not ours to
    // clear: refuse it rather than remove directories outside the repository.
    let (Ok(real_base), Ok(real_root)) = (
        std::fs::canonicalize(&base),
        std::fs::canonicalize(repo_root),
    ) else {
        return Vec::new();
    };
    if !real_base.starts_with(&real_root) {
        return vec![(
            base.clone(),
            Err(PatchError::OutsideWorktree(format!(
                "{} resolves to {}, outside the repository; nothing was swept",
                base.display(),
                real_base.display()
            ))),
        )];
    }
    let Ok(entries) = std::fs::read_dir(&base) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            // A link named like a worktree is skipped: only real directories
            // inside the worktrees root are removed.
            let is_link = std::fs::symlink_metadata(path)
                .map(|meta| meta.file_type().is_symlink())
                .unwrap_or(true);
            !is_link
                && path.is_dir()
                && std::fs::canonicalize(path).is_ok_and(|real| real.starts_with(&real_base))
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
        if self.remove_on_drop && !self.removed {
            let _ = self.remove();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "subprocess helper invoked by static_git_never_selects_a_project_executable"]
    fn git_lookup_worker() {
        let Some(root) = std::env::var_os("LOCALPILOT_GIT_LOOKUP_ROOT") else {
            return;
        };
        let version = git(Path::new(&root), &["--version"]).unwrap();
        assert!(version.starts_with("git version"));
    }

    #[test]
    fn static_git_never_selects_a_project_executable() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("marker.rs");
        std::fs::write(&source, "fn main() { if let Some(p) = std::env::var_os(\"LOCALPILOT_GIT_LOOKUP_MARKER\") { std::fs::write(p, b\"executed\").unwrap(); } println!(\"git version controlled-fake\"); }\n").unwrap();
        let fake_git = root
            .path()
            .join(if cfg!(windows) { "git.exe" } else { "git" });
        let built = Command::new("rustc")
            .args(["--edition=2021", "--crate-name", "controlled_git_marker"])
            .arg(&source)
            .arg("-o")
            .arg(&fake_git)
            .output()
            .unwrap();
        assert!(
            built.status.success(),
            "{}",
            String::from_utf8_lossy(&built.stderr)
        );
        let marker = root.path().join("executed-marker");
        Command::new(&fake_git)
            .env("LOCALPILOT_GIT_LOOKUP_MARKER", &marker)
            .output()
            .unwrap();
        assert!(
            marker.is_file(),
            "positive control must execute the marker fixture"
        );
        std::fs::remove_file(&marker).unwrap();
        let mut entries = vec![root.path().to_path_buf()];
        entries.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "git::tests::git_lookup_worker",
                "--ignored",
                "--nocapture",
            ])
            .current_dir(root.path())
            .env("PATH", std::env::join_paths(entries).unwrap())
            .env("LOCALPILOT_GIT_LOOKUP_ROOT", root.path())
            .env("LOCALPILOT_GIT_LOOKUP_MARKER", &marker)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !marker.exists(),
            "static Git executed a project-controlled executable before consent"
        );
    }

    #[cfg(windows)]
    #[test]
    fn proposal_write_path_refuses_a_directory_junction() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = root.path().join("escape");
        let output = Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&link)
            .arg(outside.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(matches!(
            contained_edit_path(root.path(), "escape/new.txt"),
            Err(PatchError::OutsideWorktree(_))
        ));
        assert!(!outside.path().join("new.txt").exists());
        assert!(link.starts_with(root.path()));
        std::fs::remove_dir(link).unwrap();
    }

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
