//! What a stored version records about itself.
//!
//! One JSON file beside each installed binary, written last so a torn install is
//! never resolvable. It mirrors the *idea* of `localpilot-dist`'s install marker
//! — presence means "resolvable", contents mean "here is what this is" — but the
//! fields are a self-dev build's, not a release's: a source label rather than a
//! semver, and the identity the build embedded so a later step can check the
//! binary against the tree it claims to come from.

use serde::{Deserialize, Serialize};

/// The marker format this build writes and understands.
pub const BUILD_MARKER_VERSION: u32 = 1;
/// File name of the marker inside a version directory.
pub(crate) const MARKER_FILE: &str = ".selfdev.json";

/// The record written beside a stored self-dev binary. Its presence is what
/// makes a version resolvable; its contents are what the gauntlet and the
/// version comparison read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildMarker {
    /// Marker format version, so a future change is detectable, not misread.
    pub marker_version: u32,
    /// The source label this was built from — the directory name, too.
    pub label: String,
    /// The commit the source tree was at, or `unknown`.
    pub git_hash: String,
    /// The source fingerprint the binary was built with (subject 01).
    pub fingerprint: String,
    /// Whether the source tree was dirty at build time.
    pub dirty: bool,
    /// The version string the binary reports (`version --json`).
    pub version: String,
    /// The executable's file name inside this directory.
    pub executable: String,
}

impl BuildMarker {
    /// A marker for `label` carrying the identity a build embedded.
    #[must_use]
    pub fn new(
        label: impl Into<String>,
        git_hash: impl Into<String>,
        fingerprint: impl Into<String>,
        dirty: bool,
        version: impl Into<String>,
        executable: impl Into<String>,
    ) -> Self {
        Self {
            marker_version: BUILD_MARKER_VERSION,
            label: label.into(),
            git_hash: git_hash.into(),
            fingerprint: fingerprint.into(),
            dirty,
            version: version.into(),
            executable: executable.into(),
        }
    }
}
