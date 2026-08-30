//! Which swarm a directory belongs to.
//!
//! Two sessions started in the same repository should find each other; two
//! started in unrelated directories should not. The question sounds like "same
//! path?" and is not: a git *worktree* has its own path but is the same
//! repository, and a coordinator that spawns a worker into a worktree would
//! otherwise create a second, invisible swarm and then wait forever for a member
//! that is not in it.
//!
//! So the identity is the repository's **common directory** — the one `.git`
//! that every worktree of a repo shares — resolved without shelling out to git,
//! because a swarm id is needed on every spawn and a process launch per lookup
//! is not a reasonable price. Outside a repository the canonical directory path
//! is used instead, which keeps non-git workspaces working rather than making
//! them a special case that errors.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Overrides the resolved swarm, for a caller that wants two directories to
/// share one swarm (or one directory to host two).
pub const SWARM_ID_ENV: &str = "LOCALPILOT_SWARM_ID";

/// Identifies one swarm.
///
/// Opaque on purpose: it is a *key*, and callers that parsed it would couple
/// themselves to the resolution rules below.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SwarmId(String);

impl SwarmId {
    /// Wrap an identifier. Public because the env override and a restored
    /// snapshot both name a swarm that was resolved earlier.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The identifier as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SwarmId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Resolve the swarm `dir` belongs to.
///
/// In order: the [`SWARM_ID_ENV`] override, then the repository common directory
/// (so every worktree of one repo lands in one swarm), then the canonical path.
/// Never fails — an unreadable or non-existent path degrades to its own
/// swarm rather than to an error, because refusing to resolve would take down
/// the spawn path over a directory question.
#[must_use]
pub fn swarm_id_for_dir(dir: &Path) -> SwarmId {
    swarm_id_for_dir_below(dir, None)
}

/// [`swarm_id_for_dir`], with the repository search bounded above `ceiling`.
/// See [`git_common_dir_below`] for why the bound exists.
fn swarm_id_for_dir_below(dir: &Path, ceiling: Option<&Path>) -> SwarmId {
    if let Ok(value) = std::env::var(SWARM_ID_ENV) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return SwarmId::new(trimmed);
        }
    }
    let canonical = canonical(dir);
    match git_common_dir_below(&canonical, ceiling) {
        Some(common) => SwarmId::new(format!("git-{}", key(&common))),
        None => SwarmId::new(format!("dir-{}", key(&canonical))),
    }
}

/// The `.git` directory shared by every worktree of the repository containing
/// `start`, or `None` outside a repository.
///
/// Resolved by reading git's own on-disk contract rather than by running git:
///
/// - `<dir>/.git` as a **directory** is an ordinary checkout; that is the git
///   directory.
/// - `<dir>/.git` as a **file** is a linked worktree (or a submodule). It holds
///   `gitdir: <path>`, possibly relative, pointing at this worktree's private
///   git directory.
/// - a private git directory holds a `commondir` file naming the shared one,
///   again possibly relative. Its absence means the git directory *is* the
///   shared one.
///
/// Reading `commondir` rather than assuming `<common>/worktrees/<name>` matters:
/// the layout is git's business, the file is the documented answer, and a
/// submodule's git directory is nested somewhere else entirely.
#[must_use]
pub fn git_common_dir(start: &Path) -> Option<PathBuf> {
    git_common_dir_below(start, None)
}

/// [`git_common_dir`], stopping above `ceiling` rather than at the filesystem
/// root.
///
/// The unbounded walk is the right default — a workspace is routinely a
/// subdirectory of the repository it belongs to — but it does mean the answer
/// depends on the whole ancestor chain, including directories the caller has
/// never heard of. This exists so a test can pin the no-repository path without
/// depending on whether some ancestor of the temp directory happens to be a
/// checkout, which on a developer's machine it sometimes is.
fn git_common_dir_below(start: &Path, ceiling: Option<&Path>) -> Option<PathBuf> {
    let ceiling = ceiling.map(canonical);
    let mut current = Some(start);
    while let Some(dir) = current {
        let marker = dir.join(".git");
        if marker.is_dir() {
            return Some(common_of(&marker));
        }
        if marker.is_file() {
            let gitdir = read_pointer(&marker, "gitdir:")?;
            let gitdir = resolve_against(dir, &gitdir);
            return Some(common_of(&gitdir));
        }
        if ceiling.as_deref().is_some_and(|top| canonical(dir) == top) {
            return None;
        }
        current = dir.parent();
    }
    None
}

/// Follow a git directory's `commondir`, if it has one.
fn common_of(gitdir: &Path) -> PathBuf {
    let pointer = gitdir.join("commondir");
    if let Some(target) = read_pointer(&pointer, "") {
        return canonical(&resolve_against(gitdir, &target));
    }
    canonical(gitdir)
}

/// Read a one-line git pointer file, stripping `prefix` if present.
///
/// Returns `None` for a missing or unreadable file, and for an empty payload —
/// a truncated pointer is a repository we cannot identify, and guessing would be
/// worse than falling back to the path.
fn read_pointer(path: &Path, prefix: &str) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let line = raw.lines().next()?.trim();
    let payload = if prefix.is_empty() {
        line
    } else {
        line.strip_prefix(prefix)?.trim()
    };
    if payload.is_empty() {
        return None;
    }
    Some(payload.to_string())
}

/// Resolve `target` against `base` when it is relative. Git writes both forms,
/// and on Windows it writes forward slashes either way — which `Path` handles,
/// but only once the string is treated as a path rather than compared as text.
fn resolve_against(base: &Path, target: &str) -> PathBuf {
    let target = Path::new(target);
    if target.is_absolute() {
        target.to_path_buf()
    } else {
        base.join(target)
    }
}

/// Canonicalize, falling back to the path as given. A path that cannot be
/// canonicalized (it does not exist yet, or permissions refuse) still deserves a
/// stable id.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// A short, stable hex key for a path.
///
/// FNV-1a over the path bytes, matching how the transport keys its endpoint: no
/// clock, no randomness, no hash crate, and identical across processes and
/// platform versions — which is the whole requirement, since two processes have
/// to agree on it without talking to each other.
fn key(path: &Path) -> String {
    format!(
        "{:016x}",
        crate::transport::fnv1a_64(path.to_string_lossy().as_bytes())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `LOCALPILOT_SWARM_ID` is process-global, and `cargo test` runs these in
    /// one process on many threads. *Every* test that resolves a swarm id takes
    /// this lock — not only the two that set the variable — because a test that
    /// merely reads it is just as much a participant in the race.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Take the environment lock, ignoring poisoning: a panicking test has
    /// already failed, and refusing to run the rest would hide their results
    /// behind its own.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn a_plain_checkout_resolves_to_its_own_git_directory() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("crates/thing")).unwrap();

        let common = git_common_dir(&repo).expect("a checkout is a repository");
        assert_eq!(common, std::fs::canonicalize(repo.join(".git")).unwrap());

        // A nested directory finds the same repository by walking up.
        assert_eq!(git_common_dir(&repo.join("crates/thing")).unwrap(), common);
    }

    #[test]
    fn two_worktrees_of_one_repository_share_a_swarm() {
        let _env = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let gitdir = repo.join(".git");
        std::fs::create_dir_all(&gitdir).unwrap();

        // A linked worktree: `.git` is a file pointing at a private git dir
        // under the main repository, which names the common dir relatively —
        // exactly what git writes.
        let worktree = temp.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        let private = gitdir.join("worktrees/wt");
        write(&private.join("commondir"), "../..\n");
        write(
            &worktree.join(".git"),
            &format!("gitdir: {}\n", private.display()),
        );

        let from_repo = swarm_id_for_dir(&repo);
        let from_worktree = swarm_id_for_dir(&worktree);
        assert_eq!(
            from_repo, from_worktree,
            "a worktree is the same repository, so it is the same swarm"
        );
    }

    #[test]
    fn a_relative_gitdir_pointer_resolves_against_the_worktree() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        let worktree = root.join("repo/nested");
        std::fs::create_dir_all(&worktree).unwrap();
        // Relative pointer, forward slashes — how git writes it on every OS.
        write(&worktree.join(".git"), "gitdir: ../.git\n");

        assert_eq!(
            git_common_dir(&worktree).unwrap(),
            git_common_dir(&root.join("repo")).unwrap()
        );
    }

    #[test]
    fn a_git_directory_without_a_commondir_file_is_its_own_common_directory() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        assert_eq!(
            git_common_dir(&repo).unwrap(),
            std::fs::canonicalize(repo.join(".git")).unwrap()
        );
    }

    #[test]
    fn unrelated_repositories_get_different_swarms() {
        let _env = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let one = temp.path().join("one");
        let two = temp.path().join("two");
        std::fs::create_dir_all(one.join(".git")).unwrap();
        std::fs::create_dir_all(two.join(".git")).unwrap();
        assert_ne!(swarm_id_for_dir(&one), swarm_id_for_dir(&two));
    }

    #[test]
    fn a_directory_outside_any_repository_gets_a_path_scoped_swarm() {
        let temp = tempfile::tempdir().unwrap();
        let plain = temp.path().join("not-a-repo");
        std::fs::create_dir_all(&plain).unwrap();

        // Bounded at the temp root: an ancestor of the system temp directory is
        // occasionally itself a checkout on a developer's machine, and this test
        // is about the no-repository path, not about that.
        assert!(git_common_dir_below(&plain, Some(temp.path())).is_none());
    }

    #[test]
    fn a_path_scoped_swarm_is_stable_and_distinct() {
        let _env = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let one = temp.path().join("one");
        let two = temp.path().join("two");
        std::fs::create_dir_all(&one).unwrap();
        std::fs::create_dir_all(&two).unwrap();

        let id = |p: &Path| swarm_id_for_dir_below(p, Some(temp.path()));
        assert!(id(&one).as_str().starts_with("dir-"), "{}", id(&one));
        assert_eq!(id(&one), id(&one), "and it is stable");
        assert_ne!(id(&one), id(&two));
    }

    #[test]
    fn a_workspace_nested_deep_inside_a_repository_still_finds_it() {
        let _env = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let deep = repo.join("crates/thing/src");
        std::fs::create_dir_all(&deep).unwrap();

        assert_eq!(
            swarm_id_for_dir(&deep),
            swarm_id_for_dir(&repo),
            "the walk is unbounded on purpose: a workspace is routinely a \
             subdirectory of its repository"
        );
    }

    #[test]
    fn a_truncated_gitdir_pointer_falls_back_to_the_path() {
        let _env = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let worktree = temp.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        write(&worktree.join(".git"), "gitdir:\n");

        assert!(git_common_dir(&worktree).is_none());
        assert!(swarm_id_for_dir_below(&worktree, Some(temp.path()))
            .as_str()
            .starts_with("dir-"));
    }

    #[test]
    fn the_environment_override_wins_over_everything() {
        let _env = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        std::env::set_var(SWARM_ID_ENV, "  chosen-by-hand  ");
        let overridden = swarm_id_for_dir(&repo);
        std::env::remove_var(SWARM_ID_ENV);

        assert_eq!(overridden, SwarmId::new("chosen-by-hand"));
        assert_ne!(overridden, swarm_id_for_dir(&repo));
    }

    #[test]
    fn a_blank_override_is_ignored_rather_than_obeyed() {
        let _env = env_guard();
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        let natural = swarm_id_for_dir(&repo);
        std::env::set_var(SWARM_ID_ENV, "   ");
        let with_blank = swarm_id_for_dir(&repo);
        std::env::remove_var(SWARM_ID_ENV);

        assert_eq!(natural, with_blank);
    }
}
