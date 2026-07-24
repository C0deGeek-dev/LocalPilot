//! Skills error type.

/// Errors from loading or parsing skills, and from managing skill sources and
/// installations (LocalHub#40).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SkillError {
    /// A `skill.toml` manifest was invalid; the message names the bad field.
    #[error("invalid skill manifest: {0}")]
    InvalidManifest(String),

    /// A prompt template was invalid or could not be rendered.
    #[error("invalid prompt template: {0}")]
    InvalidTemplate(String),

    /// A filesystem operation failed.
    #[error("skills io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// A supplied value was rejected before any effect ran — a non-HTTPS URL,
    /// embedded credentials, path traversal, an escaping symlink, or content
    /// that exceeds a safety bound. Never a network or filesystem failure; the
    /// input itself is not allowed.
    #[error("rejected: {0}")]
    Rejected(String),

    /// A source registry or on-disk file could not be parsed as the expected
    /// TOML shape.
    #[error("corrupt skills state: {0}")]
    Corrupt(String),

    /// The requested source, package, or installed skill does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// The operation would collide with existing state — re-adding a registered
    /// source, or installing over a skill that already exists in the same scope.
    #[error("conflict: {0}")]
    Conflict(String),

    /// A mutation was refused by policy rather than by input: an untrusted
    /// workspace, a missing home directory for a global mutation, an
    /// unattended run without explicit approval, or a delete of a skill
    /// LocalPilot did not install.
    #[error("refused: {0}")]
    Refused(String),

    /// A network operation (fetching or refreshing a repository) failed. The
    /// previous cache, if any, is left intact.
    #[error("fetch failed: {0}")]
    Fetch(String),
}
