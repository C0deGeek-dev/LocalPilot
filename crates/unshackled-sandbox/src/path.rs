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
        Ok(Self { root })
    }

    /// The canonicalized workspace root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
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
}
