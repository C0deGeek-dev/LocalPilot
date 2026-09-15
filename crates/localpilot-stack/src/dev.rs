//! The development channel: a pinned LocalX workspace of repository checkouts,
//! built with cargo instead of downloaded from a release.
//!
//! Someone developing the stack runs the code in their own working tree, not the
//! last cut release and not the last pushed commit. That is a *mode*, not a flag
//! to remember on every command: the workspace is pinned once and every later
//! `install`/`update` builds from it until the pin is removed.
//!
//! The pin lives beside the install cache (`<localx root>/dev.json`) so it
//! survives a rebuild of every binary in the stack, and is a single file to
//! delete when the mode is no longer wanted.

use std::path::{Path, PathBuf};

use localpilot_dist::Cache;

use crate::{StackTool, TRAIN};

/// The pin file's name, inside the `localx` data directory.
const PIN_FILE: &str = "dev.json";

/// The pin file's format version, so a future shape change is a recognised
/// mismatch rather than a silent misread.
const PIN_VERSION: u32 = 1;

/// Where the workspace pin is recorded, when the platform reports a per-user
/// data directory.
#[must_use]
pub fn pin_path() -> Option<PathBuf> {
    Cache::default_root("localx").map(|root| root.join(PIN_FILE))
}

/// The pinned workspace root, when development mode is on.
///
/// `None` when no pin is recorded, when the file is unreadable or of an unknown
/// format version, or when the directory it names is gone — a pin that no longer
/// resolves must not silently become "some other directory".
#[must_use]
pub fn pinned() -> Option<PathBuf> {
    let text = std::fs::read_to_string(pin_path()?).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    if value.get("version").and_then(serde_json::Value::as_u64) != Some(u64::from(PIN_VERSION)) {
        return None;
    }
    let root = PathBuf::from(value.get("workspace")?.as_str()?);
    root.is_dir().then_some(root)
}

/// Record `root` as the workspace every later install builds from.
///
/// # Errors
/// Returns an error when the workspace does not hold every train repository, or
/// when the pin cannot be written.
pub fn pin(root: &Path) -> anyhow::Result<PathBuf> {
    // `canonicalize` hands back a `\\?\`-prefixed path on Windows. cargo accepts
    // it, but every message that prints the workspace is worse for carrying it,
    // and a pin is read by people far more often than by code.
    let root = dunce_like(&std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()));
    let missing = missing_crates(&root);
    if !missing.is_empty() {
        anyhow::bail!(
            "{} is not a LocalX workspace; it is missing: {}",
            root.display(),
            missing.join(", ")
        );
    }
    let path = pin_path().ok_or_else(|| {
        anyhow::anyhow!("no per-user data directory on this platform, so the pin cannot be stored")
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let document = serde_json::json!({
        "version": PIN_VERSION,
        "workspace": root.display().to_string(),
    });
    std::fs::write(&path, format!("{document:#}\n"))?;
    Ok(root)
}

/// Remove the workspace pin, returning whether there was one to remove.
///
/// # Errors
/// Returns an error when the pin file exists but cannot be removed.
pub fn unpin() -> anyhow::Result<bool> {
    let Some(path) = pin_path() else {
        return Ok(false);
    };
    if !path.exists() {
        return Ok(false);
    }
    std::fs::remove_file(&path)?;
    Ok(true)
}

/// The LocalX workspace at or above `start`, if there is one.
///
/// A developer runs `localx dev use` from inside their workspace far more often
/// than they type its path, so the path argument is optional and this answers
/// the common case.
#[must_use]
pub fn detect(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| missing_crates(dir).is_empty())
        .map(Path::to_path_buf)
}

/// The crate directory a train tool is built from inside `workspace`.
#[must_use]
pub fn crate_dir(workspace: &Path, tool: &StackTool) -> PathBuf {
    workspace.join(tool.repo_dir).join(tool.crate_dir)
}

/// Which train tools `workspace` cannot build, named by the manifest that is
/// missing. Empty means every tool in the train is present.
#[must_use]
pub fn missing_crates(workspace: &Path) -> Vec<String> {
    TRAIN
        .iter()
        .filter(|tool| !crate_dir(workspace, tool).join("Cargo.toml").is_file())
        .map(|tool| {
            format!(
                "{}/{}/Cargo.toml",
                tool.repo_dir,
                tool.crate_dir.replace('\\', "/")
            )
        })
        .collect()
}

/// Strip a Windows `\\?\` verbatim prefix, which `canonicalize` adds and which
/// every human-facing message is worse for carrying.
fn dunce_like(path: &Path) -> PathBuf {
    let text = path.display().to_string();
    match text.strip_prefix(r"\\?\") {
        Some(stripped) => PathBuf::from(stripped),
        None => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::{crate_dir, detect, dunce_like, missing_crates};
    use crate::TRAIN;
    use std::path::{Path, PathBuf};

    /// A directory tree that looks like a LocalX workspace: every train tool's
    /// manifest, and nothing else.
    fn workspace(root: &Path) {
        for tool in TRAIN {
            let dir = crate_dir(root, tool);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("Cargo.toml"), "[package]\n").unwrap();
        }
    }

    #[test]
    fn a_complete_workspace_is_missing_nothing() {
        let dir = tempfile::tempdir().unwrap();
        workspace(dir.path());
        assert!(missing_crates(dir.path()).is_empty());
    }

    #[test]
    fn a_missing_manifest_is_named_by_its_path() {
        let dir = tempfile::tempdir().unwrap();
        workspace(dir.path());
        let localbox = TRAIN.iter().find(|t| t.tool == "localbox").unwrap();
        std::fs::remove_file(crate_dir(dir.path(), localbox).join("Cargo.toml")).unwrap();
        assert_eq!(
            missing_crates(dir.path()),
            vec!["LocalBox/crates/localbox/Cargo.toml".to_string()]
        );
    }

    #[test]
    fn detection_walks_up_from_a_repository_to_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        workspace(dir.path());
        let inside = dir.path().join("LocalPilot").join("crates");
        assert_eq!(detect(&inside).as_deref(), Some(dir.path()));
    }

    #[test]
    fn detection_finds_nothing_outside_a_workspace() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(detect(dir.path()), None);
    }

    #[test]
    fn a_verbatim_prefix_is_stripped_for_display() {
        assert_eq!(
            dunce_like(&PathBuf::from(r"\\?\D:\repos\LocalX")),
            PathBuf::from(r"D:\repos\LocalX")
        );
        assert_eq!(
            dunce_like(&PathBuf::from("/repos/LocalX")),
            PathBuf::from("/repos/LocalX")
        );
    }
}
