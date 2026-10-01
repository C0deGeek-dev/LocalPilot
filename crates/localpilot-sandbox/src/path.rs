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
use std::sync::Arc;

use crate::error::SandboxError;

/// Creation policy for a private session directory; a custom path is its
/// parent, never a grant to the parent's existing contents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ScratchRoot {
    #[default]
    OsTemp,
    Disabled,
    Parent(PathBuf),
}

#[derive(Debug)]
struct Scratch {
    canonical: PathBuf,
    _owner: tempfile::TempDir,
}

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
    scratch_policy: ScratchRoot,
    scratch: Option<Arc<Scratch>>,
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
            scratch_policy: ScratchRoot::default(),
            scratch: None,
        })
    }

    /// Select where the next session creates scratch. Changing policy revokes
    /// this workspace's current scratch grant; other live clones retain theirs.
    pub fn set_scratch_root(&mut self, policy: ScratchRoot) {
        self.clear_scratch();
        self.scratch_policy = policy;
    }

    /// Create a unique owned child for this session, never adopting an existing
    /// directory. Failure leaves no scratch grant. Clones share its lifetime;
    /// a child session initializes its own root rather than sharing authority.
    ///
    /// # Errors
    /// Returns an I/O error for an invalid parent or failed creation/canonicalization.
    pub fn start_scratch(&mut self, session: &str) -> std::io::Result<()> {
        self.clear_scratch();
        let parent = match &self.scratch_policy {
            ScratchRoot::Disabled => return Ok(()),
            ScratchRoot::OsTemp => std::env::temp_dir(),
            ScratchRoot::Parent(parent) if parent.is_absolute() => parent.clone(),
            ScratchRoot::Parent(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "scratch parent must be an absolute existing directory",
                ));
            }
        };
        let parent = std::fs::canonicalize(parent)?;
        let label: String = session
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .take(80)
            .collect();
        let mut builder = tempfile::Builder::new();
        let prefix = format!("localpilot-{label}-");
        builder.prefix(&prefix);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let owner = builder.tempdir_in(parent)?;
        let canonical = std::fs::canonicalize(owner.path())?;
        self.scratch = Some(Arc::new(Scratch {
            canonical,
            _owner: owner,
        }));
        Ok(())
    }

    /// Revoke this grant and release the owned directory. The last live clone
    /// removes it; no custom parent or other session's directory is removed.
    pub fn clear_scratch(&mut self) {
        self.scratch = None;
    }

    /// Canonical scope of the currently owned scratch directory.
    #[must_use]
    pub fn scratch_dir(&self) -> Option<&Path> {
        self.scratch
            .as_ref()
            .map(|scratch| scratch.canonical.as_path())
    }

    /// Whether a normalized candidate remains in this session's scratch,
    /// including a not-yet-created child. Symlinks out confer no authority.
    #[must_use]
    pub fn scratch_contains(&self, candidate: &Path) -> bool {
        self.scratch_dir().is_some_and(|root| {
            self.normalize(candidate)
                .is_ok_and(|candidate| candidate.starts_with(root))
        })
    }

    /// Child-process spelling, never used for security containment.
    #[must_use]
    pub fn scratch_process_dir(&self) -> Option<PathBuf> {
        self.scratch_dir()
            .map(|path| dunce::simplified(path).to_path_buf())
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

    #[test]
    fn scratch_is_unique_scoped_and_owned_without_removing_the_parent() {
        let parent = tempfile::tempdir().unwrap();
        let (_dir, mut ws) = workspace();
        ws.set_scratch_root(ScratchRoot::Parent(parent.path().to_path_buf()));
        ws.start_scratch("session").unwrap();
        let first = ws.scratch_dir().unwrap().to_path_buf();
        std::fs::write(first.join("fixture"), "data").unwrap();
        assert!(ws.scratch_contains(&first.join("new")));
        assert!(!ws.contains(&first.join("new")));
        assert!(ws.resolve(&first.join("new")).is_err());
        assert!(!ws.scratch_contains(parent.path()));
        let clone = ws.clone();
        ws.start_scratch("session").unwrap();
        assert_ne!(first, ws.scratch_dir().unwrap());
        assert!(!ws.scratch_contains(&first));
        assert!(first.exists());
        drop(clone);
        assert!(!first.exists());
        let second = ws.scratch_dir().unwrap().to_path_buf();
        ws.clear_scratch();
        assert!(!second.exists());
        assert!(parent.path().exists());
        ws.set_scratch_root(ScratchRoot::Disabled);
        ws.start_scratch("off").unwrap();
        assert!(ws.scratch_dir().is_none());
        ws.set_scratch_root(ScratchRoot::Parent(parent.path().join("missing")));
        assert!(ws.start_scratch("bad").is_err());
        assert!(ws.scratch_dir().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn scratch_rejects_symlink_escape_and_is_owner_only() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let (_dir, mut ws) = workspace();
        ws.start_scratch("symlink").unwrap();
        let root = ws.scratch_dir().unwrap();
        assert_eq!(
            std::fs::metadata(root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let other = tempfile::tempdir().unwrap();
        symlink(other.path(), root.join("escape")).unwrap();
        assert!(!ws.scratch_contains(&root.join("escape/new")));
    }

    #[cfg(windows)]
    #[test]
    fn scratch_rejects_a_directory_junction_escape() {
        let (_dir, mut ws) = workspace();
        ws.start_scratch("junction").unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = ws.scratch_dir().unwrap().join("escape");
        let status = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(dunce::simplified(&link))
            .arg(outside.path())
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "{}",
            String::from_utf8_lossy(&status.stderr)
        );
        assert!(!ws.scratch_contains(&link.join("new")));
        ws.clear_scratch();
        assert!(outside.path().exists());
    }

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
