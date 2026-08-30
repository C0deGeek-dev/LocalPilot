//! The release manifest a published release carries.
//!
//! CI writes this file beside the archives (`manifest.json`); an updater reads it
//! to learn which targets a release actually shipped and what each archive should
//! hash to. Reading a manifest is what replaces guessing an archive name from a
//! naming convention — a convention drifts silently, a manifest does not.
//!
//! The shape is pinned by tests against the real published file, so a change in
//! CI that breaks the contract fails here rather than in a user's updater.

use serde::{Deserialize, Serialize};

use crate::error::DistError;

/// The manifest format this build understands.
pub const MANIFEST_VERSION: u32 = 1;

/// One published archive.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Artifact {
    /// Rust target triple this archive was built for.
    pub target: String,
    /// File name as attached to the release.
    pub file: String,
    /// Size in bytes, as published.
    pub size: u64,
    /// Lowercase hex SHA-256 of the archive.
    pub sha256: String,
}

/// A release's machine-readable index.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReleaseManifest {
    pub manifest_version: u32,
    /// The tool this release is for (`localpilot`, `localmind`, …).
    pub tool: String,
    /// The release version, without a leading `v`.
    pub version: String,
    pub artifacts: Vec<Artifact>,
}

impl ReleaseManifest {
    /// Parse and validate a manifest.
    ///
    /// # Errors
    /// Returns [`DistError::Manifest`] for malformed JSON, an unknown format
    /// version, or a manifest with no artefacts.
    pub fn parse(text: &str) -> Result<Self, DistError> {
        let manifest: Self =
            serde_json::from_str(text).map_err(|e| DistError::Manifest(e.to_string()))?;
        if manifest.manifest_version != MANIFEST_VERSION {
            return Err(DistError::Manifest(format!(
                "manifest_version {} is not supported; this build understands {MANIFEST_VERSION}",
                manifest.manifest_version
            )));
        }
        if manifest.artifacts.is_empty() {
            return Err(DistError::Manifest(
                "release lists no artefacts".to_string(),
            ));
        }
        Ok(manifest)
    }

    /// The artefact for `target`, if this release shipped one.
    #[must_use]
    pub fn artifact(&self, target: &str) -> Option<&Artifact> {
        self.artifacts.iter().find(|a| a.target == target)
    }

    /// Every target this release shipped, for a "not built for your platform"
    /// message that can say what *was* built.
    #[must_use]
    pub fn targets(&self) -> Vec<&str> {
        self.artifacts.iter().map(|a| a.target.as_str()).collect()
    }
}

/// The target triple this build runs on, assembled from the compile-time target
/// so it always matches what CI named the archive.
#[must_use]
pub fn current_target() -> &'static str {
    // Kept explicit rather than derived: these five strings are the contract with
    // the release workflow's matrix, and a mismatch should be a visible edit.
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "x86_64-pc-windows-msvc"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-gnu"
    } else if cfg!(all(
        target_os = "linux",
        target_arch = "x86_64",
        target_env = "musl"
    )) {
        "x86_64-unknown-linux-musl"
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The manifest published on the real LocalPilot 2.5.0 release, trimmed to
    /// two artefacts. Pinning the actual shape means a CI change that breaks the
    /// contract fails here, not in someone's updater.
    const REAL: &str = r#"{
  "manifest_version": 1,
  "tool": "localpilot",
  "version": "2.5.0",
  "artifacts": [
    { "target": "aarch64-apple-darwin", "file": "localpilot-aarch64-apple-darwin.tar.gz", "size": 12479746, "sha256": "1d2cd4bdc354deab38fa2fa0f4868746abb1d6ce51d2ef32f077ff85239ead63" },
    { "target": "x86_64-pc-windows-msvc", "file": "localpilot-x86_64-pc-windows-msvc.zip", "size": 12127207, "sha256": "a313616b4b688b3f0acceb82582183214488b6d5f95981f0a1ffff134e5d4c7c" }
  ]
}"#;

    #[test]
    fn the_real_published_manifest_parses() {
        let manifest = ReleaseManifest::parse(REAL).expect("the shipped shape must parse");
        assert_eq!(manifest.tool, "localpilot");
        assert_eq!(manifest.version, "2.5.0");
        assert_eq!(manifest.artifacts.len(), 2);
    }

    #[test]
    fn an_artifact_is_found_by_target() {
        let manifest = ReleaseManifest::parse(REAL).expect("parses");
        let windows = manifest
            .artifact("x86_64-pc-windows-msvc")
            .expect("windows shipped");
        assert_eq!(windows.file, "localpilot-x86_64-pc-windows-msvc.zip");
        assert_eq!(windows.size, 12_127_207);
        assert_eq!(windows.sha256.len(), 64, "a full sha-256 in hex");
        assert!(manifest.artifact("sparc-unknown-none").is_none());
    }

    #[test]
    fn the_target_list_can_explain_what_was_built() {
        let manifest = ReleaseManifest::parse(REAL).expect("parses");
        assert_eq!(
            manifest.targets(),
            ["aarch64-apple-darwin", "x86_64-pc-windows-msvc"]
        );
    }

    #[test]
    fn an_unknown_format_version_is_refused_with_the_supported_one() {
        let text = REAL.replace("\"manifest_version\": 1", "\"manifest_version\": 7");
        let message = ReleaseManifest::parse(&text)
            .expect_err("version 7 is unsupported")
            .to_string();
        assert!(message.contains('7') && message.contains('1'), "{message}");
    }

    #[test]
    fn an_empty_release_is_refused() {
        let text = r#"{ "manifest_version": 1, "tool": "t", "version": "1.0.0", "artifacts": [] }"#;
        assert!(
            ReleaseManifest::parse(text).is_err(),
            "a release with no artefacts is not usable and should say so"
        );
    }

    #[test]
    fn malformed_json_is_refused() {
        assert!(ReleaseManifest::parse("{").is_err());
        assert!(ReleaseManifest::parse("").is_err());
    }

    #[test]
    fn this_build_knows_its_own_target() {
        let target = current_target();
        assert!(
            !target.is_empty(),
            "the test host should be one of the released targets"
        );
        // Round-trip: the name this build reports must be one CI actually builds.
        let released = [
            "x86_64-pc-windows-msvc",
            "aarch64-apple-darwin",
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-musl",
            "x86_64-unknown-linux-gnu",
        ];
        assert!(released.contains(&target), "unreleased target {target}");
    }
}
