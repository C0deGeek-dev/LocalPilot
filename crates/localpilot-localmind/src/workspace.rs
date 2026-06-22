//! Resolve the project-local LocalMind store root from a working directory.
//!
//! A LocalMind store lives at a project root, marked by `.localmind.toml` (the
//! project config) or the `.localmind/` state directory. Running a command from a
//! subdirectory should answer from the *project's* store, not silently create a
//! second empty one beside the cwd — the same way `git` resolves its repository
//! root by walking up. The cwd is the default search start; the first ancestor
//! that holds a store wins.

use std::path::{Path, PathBuf};

/// The LocalMind project config file (also the primary store-root marker).
const CONFIG_FILE: &str = crate::CONFIG_FILE;

/// The LocalMind state directory (the secondary store-root marker).
const STATE_DIR: &str = ".localmind";

/// Where a LocalMind store resolved to, relative to a starting directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreRoot {
    /// An existing store was found at this ancestor of (or at) the start dir.
    Found(PathBuf),
    /// No store exists at or above the start dir. The carried path is the start
    /// dir itself — where a write would create the first store.
    NotFound(PathBuf),
}

impl StoreRoot {
    /// The directory to operate on: the found root, or the start dir when none was
    /// found (a write creates a store there; a read treats it as "no store").
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            StoreRoot::Found(path) | StoreRoot::NotFound(path) => path,
        }
    }

    /// Whether an existing store was resolved.
    #[must_use]
    pub fn is_found(&self) -> bool {
        matches!(self, StoreRoot::Found(_))
    }
}

/// Whether `dir` is a LocalMind store root — it holds the project config or the
/// state directory.
#[must_use]
pub fn is_store_root(dir: &Path) -> bool {
    dir.join(CONFIG_FILE).is_file() || dir.join(STATE_DIR).is_dir()
}

/// Resolve the store root by walking up from `start` (git-style): the first
/// ancestor — `start` included — that [`is_store_root`] wins. Returns
/// [`StoreRoot::NotFound`] carrying `start` when no ancestor has a store.
///
/// `Path::ancestors` yields each parent up to the filesystem/drive root, so this
/// behaves identically across Windows drive roots and POSIX `/`.
#[must_use]
pub fn resolve_store_root(start: &Path) -> StoreRoot {
    for dir in start.ancestors() {
        if is_store_root(dir) {
            return StoreRoot::Found(dir.to_path_buf());
        }
    }
    StoreRoot::NotFound(start.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_to_the_nearest_ancestor_holding_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(CONFIG_FILE), "[learning]\nenabled = true\n").unwrap();
        let deep = root.join("a").join("b").join("c");
        std::fs::create_dir_all(&deep).unwrap();

        let resolved = resolve_store_root(&deep);
        assert!(
            resolved.is_found(),
            "a subdir must resolve to the root store"
        );
        assert_eq!(
            resolved.path().canonicalize().unwrap(),
            root.canonicalize().unwrap(),
        );
    }

    #[test]
    fn the_state_directory_alone_marks_a_store_root() {
        // A project that disabled injection (creating `.localmind/`) but never
        // wrote `.localmind.toml` is still a store root.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(STATE_DIR)).unwrap();
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();

        assert!(resolve_store_root(&sub).is_found());
    }

    #[test]
    fn the_nearest_ancestor_store_shadows_a_farther_one() {
        // Walk-up stops at the *nearest* store root, so a subdir of an inner
        // project resolves to that project and not to a farther ancestor (e.g. a
        // user-home store higher up the real path).
        let dir = tempfile::tempdir().unwrap();
        let outer = dir.path();
        std::fs::write(outer.join(CONFIG_FILE), "[learning]\nenabled = true\n").unwrap();
        let inner = outer.join("project");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join(CONFIG_FILE), "[learning]\nenabled = true\n").unwrap();
        let deep = inner.join("src").join("mod");
        std::fs::create_dir_all(&deep).unwrap();

        assert_eq!(
            resolve_store_root(&deep).path().canonicalize().unwrap(),
            inner.canonicalize().unwrap(),
            "the nearer store wins over a farther ancestor"
        );
    }

    #[test]
    fn resolution_is_either_a_real_store_or_not_found_carrying_the_start() {
        // The contract holds regardless of whether a *real* ancestor of the temp
        // dir (e.g. a user-home store) happens to be a store: a Found root is a
        // genuine store root, and NotFound carries the unchanged start dir.
        let dir = tempfile::tempdir().unwrap();
        let start = dir.path().join("x").join("y");
        std::fs::create_dir_all(&start).unwrap();

        match resolve_store_root(&start) {
            StoreRoot::Found(root) => {
                assert!(is_store_root(&root), "a Found root must be a real store");
            }
            StoreRoot::NotFound(carried) => {
                assert_eq!(carried, start, "NotFound carries the start dir unchanged");
            }
        }
        assert!(
            !is_store_root(&start),
            "the bare start dir is not a store root"
        );
    }

    #[test]
    fn the_start_dir_itself_can_be_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(CONFIG_FILE), "[learning]\nenabled = true\n").unwrap();
        assert!(resolve_store_root(root).is_found());
    }
}
