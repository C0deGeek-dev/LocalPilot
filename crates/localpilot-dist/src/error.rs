//! Errors from the distribution layer.

/// An error from the cache, resolver, or updater.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DistError {
    /// A filesystem operation failed.
    #[error("{0}")]
    Io(String),

    /// A caller handed over something inconsistent — a marker that disagrees
    /// with its version, a payload with no executable.
    #[error("invalid install: {0}")]
    Invalid(String),

    /// A release manifest could not be read or did not match this build's
    /// expectations.
    #[error("manifest: {0}")]
    Manifest(String),

    /// A download's digest did not match what the release published.
    #[error("checksum mismatch for {file}: expected {expected}, got {actual}")]
    Checksum {
        file: String,
        expected: String,
        actual: String,
    },
}

impl DistError {
    /// Wrap any displayable I/O failure.
    pub(crate) fn io(error: impl std::fmt::Display) -> Self {
        DistError::Io(error.to_string())
    }
}
