//! Workspace path containment.
//!
//! Containment is the core filesystem safety boundary. A naive string
//! `starts_with` is a security bug: `..` traversal, symlinks, Windows verbatim
//! (`\\?\`) prefixes, 8.3 short names, and case differences can all smuggle a
//! path outside the workspace. We defend by normalizing `.`/`..` lexically, then
//! canonicalizing the deepest existing ancestor (which resolves symlinks, 8.3
//! names, and case on the platforms that need it) before a normalized
//! `starts_with` check. The final, possibly non-existent, component (e.g. a file
//! about to be created) is appended after canonicalizing its parent.

use std::path::{Component, Path, PathBuf};

use crate::error::SandboxError;

/// A canonicalized workspace root against which candidate paths are contained.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
    /// Extra directories the user granted standing *read* scope
    /// (`[permissions] extra_read_roots`). They widen only
    /// [`Workspace::read_scoped`] — the permission engine's read decision —
    /// never [`Workspace::resolve`] or [`Workspace::contains`], which remain
    /// the hard containment boundary.
    read_roots: Vec<PathBuf>,
}

impl Workspace {
    /// Create a workspace from an existing directory, canonicalizing the root.
    ///
    /// # Errors
    /// Returns [`SandboxError::Io`] if `root` cannot be canonicalized.
    pub fn new(root: &Path) -> Result<Self, SandboxError> {
        let root = std::fs::canonicalize(root).map_err(|source| SandboxError::Io {
            path: root.display().to_string(),
            source,
        })?;
        Ok(Self {
            root,
            read_roots: Vec::new(),
        })
    }

    /// Grant standing read scope under an existing directory, canonicalizing
    /// it. The grant affects only [`Workspace::read_scoped`]; writes and the
    /// containment guarantees of [`Workspace::resolve`] are unchanged.
    ///
    /// # Errors
    /// Returns [`SandboxError::Io`] if `root` cannot be canonicalized (for
    /// example, it does not exist) — the caller should surface the bad config
    /// entry rather than silently widening or narrowing scope.
    pub fn add_read_root(&mut self, root: &Path) -> Result<(), SandboxError> {
        let root = std::fs::canonicalize(root).map_err(|source| SandboxError::Io {
            path: root.display().to_string(),
            source,
        })?;
        self.read_roots.push(root);
        Ok(())
    }

    /// The canonicalized workspace root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The workspace root in a form a child process can use as its working
    /// directory. On Windows [`Workspace::new`] canonicalizes the root to a
    /// verbatim (`\\?\C:\…`) extended-length path; a launched shell cannot `cd`
    /// into that form (cmd falls back to `C:\Windows`, PowerShell resolves
    /// relative paths against a broken `$PWD`), so every model-issued build/test
    /// command would run outside the workspace. This returns the de-verbatim
    /// form for `Command::current_dir`, leaving the verbatim [`Workspace::root`]
    /// — the security containment boundary — untouched.
    ///
    /// This is a **spawn-only** accessor: it is never used for containment.
    /// `dunce::simplified` strips the `\\?\` / `\\?\UNC\` prefix only when the
    /// resulting path is still valid; a path that genuinely needs the verbatim
    /// form (over `MAX_PATH`, reserved names, a real UNC share) is returned
    /// unchanged, so the cwd is never corrupted. On non-Windows it is a no-op.
    #[must_use]
    pub fn process_dir(&self) -> PathBuf {
        dunce::simplified(&self.root).to_path_buf()
    }

    /// Resolve a candidate path (absolute or relative to the root) to an absolute,
    /// symlink/case/8.3-normalized path **without** enforcing containment. The
    /// workspace boundary is enforced by the permission engine, which can approve
    /// an out-of-workspace access; use [`Workspace::contains`] to drive that
    /// decision and [`Workspace::resolve`] when containment must be guaranteed.
    ///
    /// # Errors
    /// Returns [`SandboxError::Io`] if canonicalizing an existing ancestor fails.
    pub fn normalize(&self, candidate: &Path) -> Result<PathBuf, SandboxError> {
        let joined = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.root.join(candidate)
        };
        let lexical = lexically_normalize(&joined);
        canonicalize_existing_prefix(&lexical).map_err(|source| SandboxError::Io {
            path: lexical.display().to_string(),
            source,
        })
    }

    /// Resolve a candidate path, guaranteeing it stays within the workspace.
    ///
    /// # Errors
    /// Returns [`SandboxError::OutsideWorkspace`] if the path escapes the root, or
    /// [`SandboxError::Io`] if canonicalization of an existing ancestor fails.
    pub fn resolve(&self, candidate: &Path) -> Result<PathBuf, SandboxError> {
        let real = self.normalize(candidate)?;
        if real.starts_with(&self.root) {
            Ok(real)
        } else {
            Err(SandboxError::OutsideWorkspace {
                path: candidate.display().to_string(),
            })
        }
    }

    /// Whether a candidate path is contained in the workspace, without erroring.
    #[must_use]
    pub fn contains(&self, candidate: &Path) -> bool {
        match self.normalize(candidate) {
            Ok(real) => real.starts_with(&self.root),
            Err(_) => false,
        }
    }

    /// Whether a candidate path is in *read* scope: inside the workspace, or
    /// under a granted extra read root. This drives the permission engine's
    /// read decision only — it must never guard a write, and
    /// [`Workspace::resolve`] never consults it.
    #[must_use]
    pub fn read_scoped(&self, candidate: &Path) -> bool {
        match self.normalize(candidate) {
            Ok(real) => {
                real.starts_with(&self.root)
                    || self.read_roots.iter().any(|root| real.starts_with(root))
            }
            Err(_) => false,
        }
    }
}

/// Resolve `.` and `..` components without touching the filesystem. `..` pops a
/// preceding normal component but is preserved when it would escape a root, so a
/// subsequent containment check can reject it.
fn lexically_normalize(path: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out.iter().collect()
}

/// Canonicalize the deepest existing ancestor of `path` and re-append any
/// trailing components that do not yet exist.
fn canonicalize_existing_prefix(path: &Path) -> std::io::Result<PathBuf> {
    let mut ancestor = path;
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if ancestor.exists() {
            let mut resolved = std::fs::canonicalize(ancestor)?;
            for component in tail.iter().rev() {
                resolved.push(component);
            }
            return Ok(resolved);
        }
        match (ancestor.file_name(), ancestor.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_os_string());
                ancestor = parent;
            }
            _ => return Ok(path.to_path_buf()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src").join("lib.rs"), b"").unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        (dir, ws)
    }

    #[test]
    fn contains_paths_inside_the_workspace() {
        let (_dir, ws) = workspace();
        assert!(ws.contains(Path::new("src/lib.rs")));
        assert!(ws.contains(Path::new("src")));
        // A not-yet-existing file inside the workspace resolves.
        assert!(ws.contains(Path::new("src/new.rs")));
    }

    #[test]
    fn rejects_parent_traversal_escapes() {
        let (_dir, ws) = workspace();
        assert!(!ws.contains(Path::new("../outside.txt")));
        assert!(!ws.contains(Path::new("src/../../outside.txt")));
        assert!(!ws.contains(Path::new("src/../..")));
    }

    #[test]
    fn rejects_absolute_paths_outside() {
        let (_dir, ws) = workspace();
        let other = tempfile::tempdir().unwrap();
        assert!(!ws.contains(other.path()));
    }

    #[test]
    fn inner_traversal_that_stays_inside_is_allowed() {
        let (_dir, ws) = workspace();
        assert!(ws.contains(Path::new("src/../src/lib.rs")));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let (dir, ws) = workspace();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"x").unwrap();
        let link = dir.path().join("escape");
        symlink(outside.path(), &link).unwrap();
        // A symlink inside the workspace pointing outside must not grant access.
        assert!(!ws.contains(Path::new("escape/secret")));
    }

    #[cfg(windows)]
    #[test]
    fn rejects_other_drive_or_root_paths() {
        let (_dir, ws) = workspace();
        // An absolute path on a system root is outside any temp workspace.
        assert!(!ws.contains(Path::new("C:\\Windows\\System32")));
    }

    #[test]
    fn read_scoped_covers_the_workspace_and_extra_read_roots_only() {
        let (_dir, mut ws) = workspace();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("notes.md"), b"x").unwrap();
        let elsewhere = tempfile::tempdir().unwrap();

        // Before the grant, outside paths are not read-scoped.
        assert!(!ws.read_scoped(&outside.path().join("notes.md")));

        ws.add_read_root(outside.path()).unwrap();

        // The workspace itself stays read-scoped.
        assert!(ws.read_scoped(Path::new("src/lib.rs")));
        // The granted root and its children are read-scoped...
        assert!(ws.read_scoped(outside.path()));
        assert!(ws.read_scoped(&outside.path().join("notes.md")));
        // ...but an unrelated directory is not.
        assert!(!ws.read_scoped(elsewhere.path()));

        // The grant never widens the hard containment boundary.
        assert!(!ws.contains(outside.path()));
        assert!(ws.resolve(outside.path()).is_err());
    }

    #[test]
    fn add_read_root_rejects_a_missing_directory() {
        let (_dir, mut ws) = workspace();
        let missing = std::env::temp_dir().join("localpilot-no-such-read-root");
        assert!(ws.add_read_root(&missing).is_err());
    }

    #[test]
    fn process_dir_points_at_the_same_workspace_directory() {
        let (_dir, ws) = workspace();
        let spawn = ws.process_dir();
        // The spawn cwd must resolve to the very same directory as the canonical
        // root — de-verbatim only changes the spelling, never the location.
        assert_eq!(
            std::fs::canonicalize(&spawn).unwrap(),
            std::fs::canonicalize(ws.root()).unwrap(),
        );
        // It is a real, usable directory (the property the launched shell needs).
        assert!(spawn.is_dir());
    }

    #[cfg(windows)]
    #[test]
    fn process_dir_strips_the_verbatim_prefix_on_a_normal_drive_path() {
        let (_dir, ws) = workspace();
        // A temp dir is an ordinary short drive path, so the verbatim root is
        // de-verbatim-able: the spawn form must drop the `\\?\` prefix that a
        // launched shell cannot `cd` into, while the containment root keeps it.
        assert!(
            ws.root().to_string_lossy().starts_with(r"\\?\"),
            "the canonical containment root stays verbatim",
        );
        assert!(
            !ws.process_dir().to_string_lossy().starts_with(r"\\?\"),
            "the spawn cwd must not be a verbatim path",
        );
    }
}
