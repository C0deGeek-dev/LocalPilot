//! Reading a repository *catalog*: choosing the one intentional skill-package
//! root inside a fetched snapshot and enumerating its packages (LocalHub#40).
//!
//! A repository may mirror the same packages under several bundle layouts. Rather
//! than surface every `SKILL.md` in the tree, exactly one catalog root is selected
//! by a fixed precedence, and only that root's immediate package directories are
//! read. A source whose selected root has no supported layout, an unparseable
//! manifest, or two packages with the same manifest name is rejected as a whole,
//! so a half-valid catalog never installs.

use std::path::{Path, PathBuf};

use crate::error::SkillError;
use crate::loader::read_manifest;

/// The conventional catalog roots, in selection precedence. The first of these
/// that exists and contains at least one package wins; a bare root `SKILL.md`
/// (a single-skill repository) is the final fallback.
pub const CATALOG_ROOTS: &[&str] = &[
    ".localpilot/skills",
    ".agents/skills",
    ".claude/skills",
    "skills",
];

/// One package found in a repository catalog: its manifest identity and where it
/// lives inside the fetched snapshot. `source_path` is the package's path relative
/// to the repository root, recorded as install provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogPackage {
    pub name: String,
    pub description: String,
    pub version: String,
    /// Absolute path to the package directory inside the fetched snapshot.
    pub dir: PathBuf,
    /// The package's path relative to the repository root, using `/` separators
    /// (`.` for a single-skill repository).
    pub source_path: String,
}

/// The resolved catalog of a repository snapshot: the one selected root and its
/// validated packages.
#[derive(Debug, Clone)]
pub struct Catalog {
    /// A short label for the selected root (e.g. `.localpilot/skills`, or
    /// `SKILL.md` for a single-skill repository), for user disclosure.
    pub root_label: String,
    pub packages: Vec<CatalogPackage>,
}

impl Catalog {
    /// Find a package by exact manifest name.
    #[must_use]
    pub fn package(&self, name: &str) -> Option<&CatalogPackage> {
        self.packages.iter().find(|p| p.name == name)
    }
}

/// Select the catalog root of a fetched repository and read its packages.
///
/// Selection takes the first [`CATALOG_ROOTS`] entry that exists and holds at
/// least one package directory (an immediate subdirectory with a `SKILL.md`),
/// falling back to a single root `SKILL.md`. The selected root is then validated
/// as a whole: every package's manifest must parse and no two may share a name.
///
/// # Errors
/// Returns [`SkillError::Rejected`] if no supported root exists, a manifest is
/// invalid, or a manifest name is duplicated within the selected root.
pub fn read_catalog(repo_root: &Path) -> Result<Catalog, SkillError> {
    for rel in CATALOG_ROOTS {
        let dir = join_rel(repo_root, rel);
        if !dir.is_dir() {
            continue;
        }
        let package_dirs = package_dirs_in(&dir)?;
        if package_dirs.is_empty() {
            // An existing-but-empty conventional root is skipped, so the next
            // location in precedence is tried (the "first non-empty" rule).
            continue;
        }
        let packages = validate_packages(repo_root, &package_dirs)?;
        return Ok(Catalog {
            root_label: (*rel).to_string(),
            packages,
        });
    }

    // Final fallback: a single-skill repository with a root `SKILL.md`.
    if repo_root.join("SKILL.md").is_file() {
        let package = read_package(repo_root, repo_root, ".")?;
        return Ok(Catalog {
            root_label: "SKILL.md".to_string(),
            packages: vec![package],
        });
    }

    Err(SkillError::Rejected(format!(
        "no supported skill catalog root (looked for {}, or a root SKILL.md)",
        CATALOG_ROOTS.join(", ")
    )))
}

/// Join a `/`-separated relative catalog root onto a repository root in a
/// platform-correct way (so `.localpilot/skills` becomes real path components on
/// every OS).
fn join_rel(repo_root: &Path, rel: &str) -> PathBuf {
    let mut path = repo_root.to_path_buf();
    for part in rel.split('/') {
        path.push(part);
    }
    path
}

/// The immediate subdirectories of `root` that contain a `SKILL.md`, sorted by
/// path for deterministic order. Never recurses — a package is one directory
/// level, so mirrored bundle trees below it are ignored by construction.
fn package_dirs_in(root: &Path) -> Result<Vec<PathBuf>, SkillError> {
    let entries = std::fs::read_dir(root).map_err(|source| SkillError::Io {
        path: root.display().to_string(),
        source,
    })?;
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if dir.join("SKILL.md").is_file() {
            dirs.push(dir);
        }
    }
    dirs.sort();
    Ok(dirs)
}

/// Validate a selected root's package directories as a whole: parse every
/// manifest and reject a duplicate manifest name.
fn validate_packages(
    repo_root: &Path,
    package_dirs: &[PathBuf],
) -> Result<Vec<CatalogPackage>, SkillError> {
    let mut packages = Vec::with_capacity(package_dirs.len());
    for dir in package_dirs {
        let source_path = relative_source_path(repo_root, dir);
        let package = read_package(repo_root, dir, &source_path)?;
        if packages
            .iter()
            .any(|p: &CatalogPackage| p.name == package.name)
        {
            return Err(SkillError::Rejected(format!(
                "duplicate skill name `{}` in the catalog root",
                package.name
            )));
        }
        packages.push(package);
    }
    Ok(packages)
}

/// Read one package directory into a [`CatalogPackage`], surfacing a parse
/// failure as a rejection of the whole source rather than a skip.
fn read_package(
    _repo_root: &Path,
    dir: &Path,
    source_path: &str,
) -> Result<CatalogPackage, SkillError> {
    let manifest = read_manifest(dir).map_err(|e| {
        SkillError::Rejected(format!("invalid skill package at `{source_path}`: {e}"))
    })?;
    Ok(CatalogPackage {
        name: manifest.name,
        description: manifest.description,
        version: manifest.version,
        dir: dir.to_path_buf(),
        source_path: source_path.to_string(),
    })
}

/// The package directory's path relative to the repository root, as a
/// `/`-separated string (falling back to the directory name if it is not nested
/// under the root, which should not happen for a real catalog).
fn relative_source_path(repo_root: &Path, dir: &Path) -> String {
    dir.strip_prefix(repo_root)
        .ok()
        .map(|rel| {
            rel.components()
                .filter_map(|c| c.as_os_str().to_str())
                .collect::<Vec<_>>()
                .join("/")
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            dir.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(".")
                .to_string()
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Write a `SKILL.md`-only package under `<repo>/<rel>/<name>`.
    fn write_package(repo: &Path, rel: &str, name: &str, manifest_name: &str) {
        let mut dir = repo.to_path_buf();
        for part in rel.split('/') {
            if !part.is_empty() {
                dir.push(part);
            }
        }
        dir.push(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {manifest_name}\ndescription: does {manifest_name}\n---\nBody.\n"),
        )
        .unwrap();
    }

    #[test]
    fn selects_localpilot_root_over_lower_precedence_mirrors() {
        let repo = tempfile::tempdir().unwrap();
        // The same package name mirrored under several roots; `.localpilot/skills`
        // must win and the mirrors must not produce duplicates.
        write_package(repo.path(), ".localpilot/skills", "a", "alpha");
        write_package(repo.path(), ".agents/skills", "a", "alpha");
        write_package(repo.path(), "skills", "a", "alpha");
        let catalog = read_catalog(repo.path()).unwrap();
        assert_eq!(catalog.root_label, ".localpilot/skills");
        assert_eq!(catalog.packages.len(), 1);
        assert_eq!(catalog.packages[0].name, "alpha");
        assert_eq!(catalog.packages[0].source_path, ".localpilot/skills/a");
    }

    #[test]
    fn skips_an_empty_root_and_uses_the_next_non_empty_one() {
        let repo = tempfile::tempdir().unwrap();
        // An empty `.localpilot/skills` exists but holds no packages.
        std::fs::create_dir_all(repo.path().join(".localpilot").join("skills")).unwrap();
        write_package(repo.path(), ".agents/skills", "b", "beta");
        let catalog = read_catalog(repo.path()).unwrap();
        assert_eq!(catalog.root_label, ".agents/skills");
        assert_eq!(catalog.packages[0].name, "beta");
    }

    #[test]
    fn a_single_root_skill_md_is_one_package() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(
            repo.path().join("SKILL.md"),
            "---\nname: solo\ndescription: a single-skill repo\n---\nBody.\n",
        )
        .unwrap();
        let catalog = read_catalog(repo.path()).unwrap();
        assert_eq!(catalog.root_label, "SKILL.md");
        assert_eq!(catalog.packages.len(), 1);
        assert_eq!(catalog.packages[0].name, "solo");
        assert_eq!(catalog.packages[0].source_path, ".");
    }

    #[test]
    fn a_duplicate_manifest_name_rejects_the_whole_source() {
        let repo = tempfile::tempdir().unwrap();
        // Two directories whose manifests declare the same name.
        write_package(repo.path(), ".localpilot/skills", "dir-one", "same");
        write_package(repo.path(), ".localpilot/skills", "dir-two", "same");
        assert!(matches!(
            read_catalog(repo.path()),
            Err(SkillError::Rejected(_))
        ));
    }

    #[test]
    fn an_invalid_manifest_rejects_the_whole_source() {
        let repo = tempfile::tempdir().unwrap();
        write_package(repo.path(), ".localpilot/skills", "good", "good");
        // A package dir with a malformed frontmatter name.
        let bad = repo.path().join(".localpilot").join("skills").join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(
            bad.join("SKILL.md"),
            "---\nname: Not Valid Name\ndescription: x\n---\nbody\n",
        )
        .unwrap();
        assert!(matches!(
            read_catalog(repo.path()),
            Err(SkillError::Rejected(_))
        ));
    }

    #[test]
    fn no_supported_root_is_rejected() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("README.md"), "not a skill").unwrap();
        assert!(matches!(
            read_catalog(repo.path()),
            Err(SkillError::Rejected(_))
        ));
    }
}
