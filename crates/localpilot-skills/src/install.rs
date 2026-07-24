//! Managed skill installs: copying a validated package snapshot into a scope's
//! `skills/` directory, recording its provenance, and removing only what
//! LocalPilot itself installed (LocalHub#40).
//!
//! An install copies the complete package tree (instructions, `skill.toml`,
//! assets, scripts) into `<scope>/.localpilot/skills/<name>`, which is already a
//! discovery root, so the skill becomes effective through the normal resolver
//! with no separate enable step. Nothing in the package is executed and no
//! permission is granted; the copy is a plain file copy under safety bounds.
//!
//! Provenance lives in a per-scope ledger (`installed-skills.toml`), *outside* the
//! third-party package content, so a later `skills delete` can prove a skill was
//! LocalPilot-installed. A skill with no ledger entry — hand-authored or checked
//! into `.agents`/`.claude`/`.localpilot` by the user — is refused, never removed.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::catalog::CatalogPackage;
use crate::error::SkillError;
use crate::fetch::TreeBudget;

/// Per-package size and file-count ceilings for a managed install. A package that
/// exceeds either is refused rather than copied, so a hostile catalog cannot fill
/// the disk through an install.
const MAX_PACKAGE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PACKAGE_FILES: usize = 4_000;

/// The recorded origin of a managed install. Kept in the scope's ledger so a
/// delete can distinguish a LocalPilot install from hand-authored content, and so
/// an installed skill remains attributable after its source is removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// The installed skill's manifest name (also its directory name).
    pub name: String,
    pub source_id: String,
    pub source_url: String,
    pub commit: String,
    /// The package's path within the source repository.
    pub source_path: String,
    /// `"project"` or `"global"` — the scope the skill was installed into.
    pub scope: String,
    /// When the install happened (an injected timestamp string, for hermetic tests).
    pub installed_at: String,
}

/// The on-disk shape of the install ledger: a TOML file with an `[[installed]]`
/// array.
#[derive(Debug, Default, Serialize, Deserialize)]
struct LedgerFile {
    #[serde(default)]
    installed: Vec<Provenance>,
}

/// A scope's record of the skills LocalPilot installed, backed by a TOML file.
#[derive(Debug, Clone)]
pub struct InstallLedger {
    entries: Vec<Provenance>,
    path: PathBuf,
}

impl InstallLedger {
    /// Load the ledger at `path`. A missing file is an empty ledger, not an error.
    ///
    /// # Errors
    /// Returns [`SkillError::Io`] if the file exists but cannot be read, or
    /// [`SkillError::Corrupt`] if it is not valid ledger TOML.
    pub fn load(path: &Path) -> Result<Self, SkillError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    entries: Vec::new(),
                    path: path.to_path_buf(),
                });
            }
            Err(source) => {
                return Err(SkillError::Io {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        let parsed: LedgerFile =
            toml::from_str(&text).map_err(|e| SkillError::Corrupt(e.to_string()))?;
        Ok(Self {
            entries: parsed.installed,
            path: path.to_path_buf(),
        })
    }

    /// The recorded installs.
    #[must_use]
    pub fn entries(&self) -> &[Provenance] {
        &self.entries
    }

    /// The provenance of an installed skill by name, if LocalPilot installed it.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Provenance> {
        self.entries.iter().find(|p| p.name == name)
    }

    /// Persist the ledger, creating the parent directory if needed.
    ///
    /// # Errors
    /// Returns [`SkillError::Io`] if the directory or file cannot be written.
    pub fn save(&self) -> Result<(), SkillError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| SkillError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }
        let file = LedgerFile {
            installed: self.entries.clone(),
        };
        let text = toml::to_string_pretty(&file)
            .map_err(|e| SkillError::Corrupt(format!("could not serialize ledger: {e}")))?;
        std::fs::write(&self.path, text).map_err(|source| SkillError::Io {
            path: self.path.display().to_string(),
            source,
        })
    }
}

/// Install one package into `skills_dir`, recording `provenance` in `ledger`.
///
/// The copy is staged into a sibling directory and atomically renamed into place,
/// so a failure never leaves a half-written skill. A same-scope skill is never
/// overwritten — whether it is a prior managed install or hand-authored content,
/// an existing directory of the same name is a conflict. The ledger is saved only
/// after the rename succeeds.
///
/// # Errors
/// - [`SkillError::Conflict`] if a skill of the same name already exists in scope.
/// - [`SkillError::Rejected`] if the package exceeds a size/file bound or contains
///   an escaping symlink.
/// - [`SkillError::Io`] on a filesystem failure.
pub fn install_package(
    skills_dir: &Path,
    ledger: &mut InstallLedger,
    package: &CatalogPackage,
    provenance: Provenance,
) -> Result<PathBuf, SkillError> {
    let target = skills_dir.join(&package.name);
    if target.exists() {
        return Err(SkillError::Conflict(format!(
            "a skill named `{}` already exists in this scope; delete it first to replace it",
            package.name
        )));
    }
    std::fs::create_dir_all(skills_dir).map_err(|source| SkillError::Io {
        path: skills_dir.display().to_string(),
        source,
    })?;

    // Stage into a sibling directory, then rename into place (atomic on one
    // filesystem), so an interrupted copy never yields a partial skill.
    let staging = skills_dir.join(format!(".localpilot-staging-{}", package.name));
    if staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    let mut budget = TreeBudget {
        max_bytes: MAX_PACKAGE_BYTES,
        max_files: MAX_PACKAGE_FILES,
        bytes: 0,
        files: 0,
    };
    if let Err(err) = copy_tree_bounded(&package.dir, &staging, &package.dir, &mut budget) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(err);
    }
    std::fs::rename(&staging, &target).map_err(|source| {
        let _ = std::fs::remove_dir_all(&staging);
        SkillError::Io {
            path: target.display().to_string(),
            source,
        }
    })?;

    ledger.entries.retain(|p| p.name != provenance.name);
    ledger.entries.push(provenance);
    ledger.save()?;
    Ok(target)
}

/// Remove a managed install by name. Only a skill recorded in `ledger` is removed;
/// a skill with no ledger entry is refused so hand-authored or checked-in content
/// is never deleted.
///
/// # Errors
/// - [`SkillError::Refused`] if `name` was not installed by LocalPilot.
/// - [`SkillError::Io`] on a filesystem failure.
pub fn delete_installed(
    skills_dir: &Path,
    ledger: &mut InstallLedger,
    name: &str,
) -> Result<PathBuf, SkillError> {
    if ledger.get(name).is_none() {
        return Err(SkillError::Refused(format!(
            "`{name}` was not installed by LocalPilot; refusing to remove hand-authored or \
             checked-in content"
        )));
    }
    let target = skills_dir.join(name);
    if target.exists() {
        std::fs::remove_dir_all(&target).map_err(|source| SkillError::Io {
            path: target.display().to_string(),
            source,
        })?;
    }
    ledger.entries.retain(|p| p.name != name);
    ledger.save()?;
    Ok(target)
}

/// Recursively copy `src` into `dst` under a running byte/file budget, rooted at
/// `root` for symlink-containment checks. A `.git` directory is skipped (a
/// single-skill repository's package directory is the repository root), an
/// escaping symlink is refused, and either exceeded bound aborts the copy.
fn copy_tree_bounded(
    src: &Path,
    dst: &Path,
    root: &Path,
    budget: &mut TreeBudget,
) -> Result<(), SkillError> {
    std::fs::create_dir_all(dst).map_err(|source| SkillError::Io {
        path: dst.display().to_string(),
        source,
    })?;
    let entries = std::fs::read_dir(src).map_err(|source| SkillError::Io {
        path: src.display().to_string(),
        source,
    })?;
    for entry in entries.flatten() {
        let from = entry.path();
        let name = entry.file_name();
        // Never copy version-control metadata into an installed skill.
        if name == std::ffi::OsStr::new(".git") {
            continue;
        }
        let meta = std::fs::symlink_metadata(&from).map_err(|source| SkillError::Io {
            path: from.display().to_string(),
            source,
        })?;
        let file_type = meta.file_type();
        if file_type.is_symlink() {
            // A symlink is refused: an escaping one is a traversal risk, and an
            // internal one is not needed by a skill package. Report an escaping
            // target distinctly for a clearer message.
            let escapes = std::fs::canonicalize(&from)
                .ok()
                .and_then(|target| {
                    std::fs::canonicalize(root)
                        .ok()
                        .map(|root_canon| !target.starts_with(&root_canon))
                })
                .unwrap_or(true);
            return Err(SkillError::Rejected(if escapes {
                format!("symlink escapes the package: {}", from.display())
            } else {
                format!(
                    "symlinks in skill packages are not supported: {}",
                    from.display()
                )
            }));
        }
        let to = dst.join(&name);
        if file_type.is_dir() {
            copy_tree_bounded(&from, &to, root, budget)?;
        } else {
            budget.files += 1;
            budget.bytes = budget.bytes.saturating_add(meta.len());
            if budget.files > budget.max_files {
                return Err(SkillError::Rejected(format!(
                    "skill package has too many files (> {})",
                    budget.max_files
                )));
            }
            if budget.bytes > budget.max_bytes {
                return Err(SkillError::Rejected(format!(
                    "skill package is too large (> {} bytes)",
                    budget.max_bytes
                )));
            }
            std::fs::copy(&from, &to).map_err(|source| SkillError::Io {
                path: to.display().to_string(),
                source,
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn package(dir: &Path, name: &str) -> CatalogPackage {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: does {name}\n---\nBody of {name}.\n"),
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("scripts").join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
        CatalogPackage {
            name: name.to_string(),
            description: format!("does {name}"),
            version: "0.0.0".to_string(),
            dir: dir.to_path_buf(),
            source_path: format!(".localpilot/skills/{name}"),
        }
    }

    fn provenance(name: &str, scope: &str) -> Provenance {
        Provenance {
            name: name.to_string(),
            source_id: "github-com-o-r".to_string(),
            source_url: "https://github.com/o/r".to_string(),
            commit: "abc123".to_string(),
            source_path: format!(".localpilot/skills/{name}"),
            scope: scope.to_string(),
            installed_at: "1000".to_string(),
        }
    }

    #[test]
    fn install_copies_the_whole_package_and_records_provenance() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src").join("helper");
        let pkg = package(&src, "helper");
        let skills_dir = tmp.path().join("skills");
        let ledger_path = tmp.path().join("installed-skills.toml");
        let mut ledger = InstallLedger::load(&ledger_path).unwrap();

        let target = install_package(
            &skills_dir,
            &mut ledger,
            &pkg,
            provenance("helper", "project"),
        )
        .unwrap();
        // The full tree — instructions and scripts — is copied, nothing executed.
        assert!(target.join("SKILL.md").is_file());
        assert!(target.join("scripts").join("run.sh").is_file());
        // Provenance is recorded and persisted.
        assert!(ledger.get("helper").is_some());
        let reloaded = InstallLedger::load(&ledger_path).unwrap();
        assert_eq!(reloaded.get("helper").unwrap().commit, "abc123");
    }

    #[test]
    fn install_never_overwrites_a_same_scope_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = package(&tmp.path().join("src").join("helper"), "helper");
        let skills_dir = tmp.path().join("skills");
        let mut ledger = InstallLedger::load(&tmp.path().join("l.toml")).unwrap();
        install_package(
            &skills_dir,
            &mut ledger,
            &pkg,
            provenance("helper", "project"),
        )
        .unwrap();
        // A second install of the same name in the same scope is a conflict.
        let err = install_package(
            &skills_dir,
            &mut ledger,
            &pkg,
            provenance("helper", "project"),
        )
        .unwrap_err();
        assert!(matches!(err, SkillError::Conflict(_)), "got {err:?}");
    }

    #[test]
    fn delete_removes_a_managed_skill_but_refuses_hand_authored() {
        let tmp = tempfile::tempdir().unwrap();
        let pkg = package(&tmp.path().join("src").join("helper"), "helper");
        let skills_dir = tmp.path().join("skills");
        let mut ledger = InstallLedger::load(&tmp.path().join("l.toml")).unwrap();
        install_package(
            &skills_dir,
            &mut ledger,
            &pkg,
            provenance("helper", "project"),
        )
        .unwrap();

        // A hand-authored skill directory with no ledger entry is refused.
        let hand = skills_dir.join("hand-made");
        std::fs::create_dir_all(&hand).unwrap();
        std::fs::write(
            hand.join("SKILL.md"),
            "---\nname: hand-made\ndescription: x\n---\nb\n",
        )
        .unwrap();
        let err = delete_installed(&skills_dir, &mut ledger, "hand-made").unwrap_err();
        assert!(matches!(err, SkillError::Refused(_)), "got {err:?}");
        assert!(hand.is_dir(), "hand-authored skill must not be removed");

        // The managed skill is removed and de-listed.
        delete_installed(&skills_dir, &mut ledger, "helper").unwrap();
        assert!(!skills_dir.join("helper").exists());
        assert!(ledger.get("helper").is_none());
    }

    #[test]
    fn delete_of_an_unknown_skill_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ledger = InstallLedger::load(&tmp.path().join("l.toml")).unwrap();
        let err = delete_installed(&tmp.path().join("skills"), &mut ledger, "nope").unwrap_err();
        assert!(matches!(err, SkillError::Refused(_)), "got {err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn install_refuses_a_package_with_an_escaping_symlink() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src").join("evil");
        let pkg = package(&src, "evil");
        // A symlink inside the package pointing outside it.
        let secret = tmp.path().join("secret.txt");
        std::fs::write(&secret, "top secret").unwrap();
        symlink(&secret, src.join("leak")).unwrap();

        let skills_dir = tmp.path().join("skills");
        let mut ledger = InstallLedger::load(&tmp.path().join("l.toml")).unwrap();
        let err = install_package(
            &skills_dir,
            &mut ledger,
            &pkg,
            provenance("evil", "project"),
        )
        .unwrap_err();
        assert!(matches!(err, SkillError::Rejected(_)), "got {err:?}");
        // The staging directory is cleaned up and nothing is installed.
        assert!(!skills_dir.join("evil").exists());
        assert!(ledger.get("evil").is_none());
    }
}
