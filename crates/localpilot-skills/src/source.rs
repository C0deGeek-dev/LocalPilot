//! Skill *sources*: user-registered public Git repositories that supply skill
//! packages, and the per-scope registry that records them (LocalHub#40).
//!
//! A source is a validated snapshot of one public HTTPS Git repository on its
//! default branch. Registering a source records a URL and one commit; it installs
//! nothing. The registry is a plain TOML file (`skill-sources.toml`) under a scope
//! base directory — one for the user-global scope (`~/.localpilot/`) and one per
//! project (`<project>/.localpilot/`).
//!
//! This module owns only the *source list* and URL identity. Fetching a snapshot
//! is the [`crate::fetch`] seam; reading a snapshot's catalog is [`crate::catalog`].

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::SkillError;

/// The maximum length of a source URL we will accept. A URL longer than this is
/// rejected rather than stored — repository URLs are short, and an unbounded
/// value is a red flag, not a real source.
const MAX_URL_LEN: usize = 512;

/// A registered skill source: one public HTTPS Git repository, pinned to the
/// commit validated when it was added or last refreshed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSource {
    /// A stable, human-readable id derived from the normalized URL. Used to
    /// disambiguate an install (`--repo <id>`) and to label listings.
    pub id: String,
    /// The normalized HTTPS URL (its `.git`/trailing-slash variants collapse to
    /// this one form, so re-adding an equivalent URL is detected).
    pub url: String,
    /// The commit of the currently cached snapshot. Updated only by an explicit
    /// refresh, never by a background operation.
    pub commit: String,
    /// When the source was first registered (an injected timestamp string, so
    /// tests stay hermetic).
    pub added_at: String,
}

/// Normalize a repository URL to its canonical identity, rejecting anything that
/// is not a plain public HTTPS URL.
///
/// The V1 contract accepts only `https://` URLs on their default branch. SSH,
/// embedded credentials, explicit refs, and non-HTTPS schemes are out of scope
/// and rejected here, before any network or filesystem effect. `.git` and
/// trailing-slash variants collapse to one form so they identify the same source.
///
/// # Errors
/// Returns [`SkillError::Rejected`] for a non-HTTPS scheme, embedded credentials,
/// a missing host or repository path, a path-traversal segment, control or
/// whitespace characters, or an over-long value.
pub fn normalize_url(raw: &str) -> Result<String, SkillError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(SkillError::Rejected("empty repository URL".to_string()));
    }
    if trimmed.len() > MAX_URL_LEN {
        return Err(SkillError::Rejected(format!(
            "repository URL is too long ({} > {MAX_URL_LEN} bytes)",
            trimmed.len()
        )));
    }
    if trimmed.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(SkillError::Rejected(
            "repository URL contains whitespace or control characters".to_string(),
        ));
    }
    // Scheme is case-insensitive; `https://` is eight bytes regardless of case.
    let scheme_len = "https://".len();
    let has_https =
        trimmed.len() >= scheme_len && trimmed[..scheme_len].eq_ignore_ascii_case("https://");
    if !has_https {
        return Err(SkillError::Rejected(format!(
            "only public https:// repository URLs are supported, got `{trimmed}`"
        )));
    }
    let after = &trimmed[scheme_len..];
    // Split authority (host[:port]) from the path at the first `/`.
    let (authority, path) = match after.find('/') {
        Some(i) => (&after[..i], &after[i..]),
        None => (after, ""),
    };
    if authority.is_empty() {
        return Err(SkillError::Rejected(
            "repository URL has no host".to_string(),
        ));
    }
    // Embedded credentials (`user:pass@host`) are never accepted.
    if authority.contains('@') {
        return Err(SkillError::Rejected(
            "repository URL must not embed credentials".to_string(),
        ));
    }
    // Host/port charset: letters, digits, dot, hyphen, and a single colon before
    // a numeric port. Anything else (a stray `?`, `#`, `\`, `~`) is rejected.
    if !authority
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
    {
        return Err(SkillError::Rejected(format!(
            "repository host `{authority}` has invalid characters"
        )));
    }
    let authority = authority.to_ascii_lowercase();

    // Path: reject traversal and query/fragment/backslash; then strip one trailing
    // slash and one trailing `.git` so equivalent URLs collapse.
    if path.contains('\\') || path.contains('?') || path.contains('#') {
        return Err(SkillError::Rejected(
            "repository URL must not contain a query, fragment, or backslash".to_string(),
        ));
    }
    let mut path = path.trim_end_matches('/');
    if let Some(stripped) = strip_git_suffix(path) {
        path = stripped;
    }
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        return Err(SkillError::Rejected(
            "repository URL has no repository path (expected https://host/owner/repo)".to_string(),
        ));
    }
    if path.split('/').any(|seg| seg == ".." || seg == ".") {
        return Err(SkillError::Rejected(
            "repository URL path must not contain `.` or `..` segments".to_string(),
        ));
    }

    Ok(format!("https://{authority}{path}"))
}

/// Strip exactly one trailing `.git` (case-insensitive) from a path, if present.
fn strip_git_suffix(path: &str) -> Option<&str> {
    let len = path.len();
    if len >= 4 && path[len - 4..].eq_ignore_ascii_case(".git") {
        Some(&path[..len - 4])
    } else {
        None
    }
}

/// Derive a stable, human-readable id from a normalized URL: the host and path,
/// lowercased, with every run of non-alphanumeric characters collapsed to a
/// single hyphen. Deterministic and filesystem-safe, so it doubles as a cache
/// directory name.
#[must_use]
pub fn source_id(normalized_url: &str) -> String {
    // Drop the fixed scheme; the identity lives in host + path.
    let body = normalized_url
        .strip_prefix("https://")
        .unwrap_or(normalized_url);
    let mut id = String::with_capacity(body.len());
    let mut last_was_sep = false;
    for c in body.chars() {
        if c.is_ascii_alphanumeric() {
            id.push(c.to_ascii_lowercase());
            last_was_sep = false;
        } else if !last_was_sep {
            id.push('-');
            last_was_sep = true;
        }
    }
    let id = id.trim_matches('-').to_string();
    // A normalized URL always has a host and path, so `id` is non-empty; guard
    // anyway so the return is never a surprise empty string.
    if id.is_empty() {
        "source".to_string()
    } else {
        // Cap the length so a pathological path cannot produce an unbounded id.
        id.chars().take(80).collect::<String>()
    }
}

/// The on-disk shape of a source registry: a TOML file with a `[[source]]` array.
#[derive(Debug, Default, Serialize, Deserialize)]
struct RegistryFile {
    #[serde(default)]
    source: Vec<SkillSource>,
}

/// The per-scope set of registered skill sources, backed by a TOML file.
#[derive(Debug, Clone)]
pub struct SourceRegistry {
    sources: Vec<SkillSource>,
    path: PathBuf,
}

impl SourceRegistry {
    /// Load the registry at `path`. A missing file is an empty registry (a scope
    /// with no sources yet), not an error.
    ///
    /// # Errors
    /// Returns [`SkillError::Io`] if the file exists but cannot be read, or
    /// [`SkillError::Corrupt`] if it is not valid registry TOML.
    pub fn load(path: &Path) -> Result<Self, SkillError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    sources: Vec::new(),
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
        let parsed: RegistryFile =
            toml::from_str(&text).map_err(|e| SkillError::Corrupt(e.to_string()))?;
        Ok(Self {
            sources: parsed.source,
            path: path.to_path_buf(),
        })
    }

    /// The registered sources, in insertion order.
    #[must_use]
    pub fn sources(&self) -> &[SkillSource] {
        &self.sources
    }

    /// Whether the registry holds no sources.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// Find a source by its id or its normalized URL. The caller passes a raw
    /// URL or an id; a raw URL is normalized before matching so `.git`/trailing
    /// variants resolve to the same source.
    #[must_use]
    pub fn find(&self, id_or_url: &str) -> Option<&SkillSource> {
        let normalized = normalize_url(id_or_url).ok();
        self.sources
            .iter()
            .find(|s| s.id == id_or_url || normalized.as_deref().is_some_and(|u| u == s.url))
    }

    /// Register a new source. Re-adding a URL that already resolves to a
    /// registered source (any `.git`/trailing-slash variant) is refused — a
    /// refresh, not a re-add, is the way to update a snapshot.
    ///
    /// # Errors
    /// Returns [`SkillError::Conflict`] if the URL or its derived id is already
    /// registered.
    pub fn add(&mut self, source: SkillSource) -> Result<(), SkillError> {
        if self.sources.iter().any(|s| s.url == source.url) {
            return Err(SkillError::Conflict(format!(
                "source `{}` is already registered; use `skills repo refresh` to update it",
                source.url
            )));
        }
        if self.sources.iter().any(|s| s.id == source.id) {
            return Err(SkillError::Conflict(format!(
                "a different source already uses the id `{}`",
                source.id
            )));
        }
        self.sources.push(source);
        Ok(())
    }

    /// Update the cached commit of a registered source (an explicit refresh).
    ///
    /// # Errors
    /// Returns [`SkillError::NotFound`] if no source matches `id_or_url`.
    pub fn set_commit(&mut self, id_or_url: &str, commit: String) -> Result<(), SkillError> {
        let normalized = normalize_url(id_or_url).ok();
        let source = self
            .sources
            .iter_mut()
            .find(|s| s.id == id_or_url || normalized.as_deref().is_some_and(|u| u == s.url))
            .ok_or_else(|| SkillError::NotFound(format!("no registered source `{id_or_url}`")))?;
        source.commit = commit;
        Ok(())
    }

    /// Remove a source by id or URL, returning the removed record. Only the
    /// registration/cache is the caller's concern here; installed skills are
    /// untouched (they carry their own provenance).
    ///
    /// # Errors
    /// Returns [`SkillError::NotFound`] if no source matches `id_or_url`.
    pub fn remove(&mut self, id_or_url: &str) -> Result<SkillSource, SkillError> {
        let normalized = normalize_url(id_or_url).ok();
        let index = self
            .sources
            .iter()
            .position(|s| s.id == id_or_url || normalized.as_deref().is_some_and(|u| u == s.url))
            .ok_or_else(|| SkillError::NotFound(format!("no registered source `{id_or_url}`")))?;
        Ok(self.sources.remove(index))
    }

    /// Persist the registry to its file, creating the parent directory if needed.
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
        let file = RegistryFile {
            source: self.sources.clone(),
        };
        let text = toml::to_string_pretty(&file)
            .map_err(|e| SkillError::Corrupt(format!("could not serialize registry: {e}")))?;
        std::fs::write(&self.path, text).map_err(|source| SkillError::Io {
            path: self.path.display().to_string(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn source(url: &str) -> SkillSource {
        let url = normalize_url(url).unwrap();
        SkillSource {
            id: source_id(&url),
            url,
            commit: "abc123".to_string(),
            added_at: "1000".to_string(),
        }
    }

    #[test]
    fn normalizes_git_and_trailing_slash_variants_to_one_form() {
        let canonical = "https://github.com/owner/repo";
        for variant in [
            "https://github.com/owner/repo",
            "https://github.com/owner/repo/",
            "https://github.com/owner/repo.git",
            "https://github.com/owner/repo.git/",
            "  https://GitHub.com/owner/repo  ",
            "HTTPS://github.com/owner/repo",
        ] {
            assert_eq!(
                normalize_url(variant).unwrap(),
                canonical,
                "variant: {variant}"
            );
        }
        // The path case is preserved (only host and scheme are lowercased).
        assert_eq!(
            normalize_url("https://example.com/Owner/Repo").unwrap(),
            "https://example.com/Owner/Repo"
        );
    }

    #[test]
    fn rejects_non_https_credentials_and_traversal() {
        assert!(matches!(
            normalize_url("http://github.com/o/r"),
            Err(SkillError::Rejected(_))
        ));
        assert!(matches!(
            normalize_url("git@github.com:o/r.git"),
            Err(SkillError::Rejected(_))
        ));
        assert!(matches!(
            normalize_url("ssh://git@github.com/o/r"),
            Err(SkillError::Rejected(_))
        ));
        assert!(matches!(
            normalize_url("https://user:pass@github.com/o/r"),
            Err(SkillError::Rejected(_))
        ));
        assert!(matches!(
            normalize_url("https://github.com/o/../../etc"),
            Err(SkillError::Rejected(_))
        ));
        assert!(matches!(
            normalize_url("https://github.com"),
            Err(SkillError::Rejected(_))
        ));
        assert!(matches!(
            normalize_url("https://github.com/o/r?ref=main"),
            Err(SkillError::Rejected(_))
        ));
    }

    #[test]
    fn source_id_is_stable_and_slug_like() {
        assert_eq!(
            source_id("https://github.com/owner/repo"),
            "github-com-owner-repo"
        );
        // The id is derived only from the normalized URL, so equivalent inputs
        // yield the same id.
        let a = normalize_url("https://github.com/owner/repo.git/").unwrap();
        let b = normalize_url("https://github.com/owner/repo").unwrap();
        assert_eq!(source_id(&a), source_id(&b));
    }

    #[test]
    fn add_refuses_a_reregistered_url_variant() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = SourceRegistry::load(&dir.path().join("skill-sources.toml")).unwrap();
        reg.add(source("https://github.com/owner/repo")).unwrap();
        // A `.git` variant of the same URL is the same source: refused.
        let dup = source("https://github.com/owner/repo.git");
        assert!(matches!(reg.add(dup), Err(SkillError::Conflict(_))));
        assert_eq!(reg.sources().len(), 1);
    }

    #[test]
    fn registry_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("skill-sources.toml");
        let mut reg = SourceRegistry::load(&path).unwrap();
        assert!(reg.is_empty());
        reg.add(source("https://github.com/owner/one")).unwrap();
        reg.add(source("https://example.com/owner/two")).unwrap();
        reg.save().unwrap();

        let reloaded = SourceRegistry::load(&path).unwrap();
        assert_eq!(reloaded.sources().len(), 2);
        assert!(reloaded.find("https://github.com/owner/one.git").is_some());
        assert!(reloaded.find("example-com-owner-two").is_some());
    }

    #[test]
    fn set_commit_and_remove_target_a_source_by_url_or_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = SourceRegistry::load(&dir.path().join("s.toml")).unwrap();
        reg.add(source("https://github.com/owner/repo")).unwrap();

        reg.set_commit("https://github.com/owner/repo/", "deadbeef".to_string())
            .unwrap();
        assert_eq!(
            reg.find("github-com-owner-repo").unwrap().commit,
            "deadbeef"
        );

        let removed = reg.remove("github-com-owner-repo").unwrap();
        assert_eq!(removed.url, "https://github.com/owner/repo");
        assert!(reg.is_empty());
        assert!(matches!(
            reg.remove("github-com-owner-repo"),
            Err(SkillError::NotFound(_))
        ));
    }

    #[test]
    fn a_missing_registry_file_loads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let reg = SourceRegistry::load(&dir.path().join("does-not-exist.toml")).unwrap();
        assert!(reg.is_empty());
    }
}
