//! Core error type.

/// Errors produced while constructing or parsing core domain values.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CoreError {
    /// A string could not be parsed into a UUID-backed identifier.
    #[error("invalid identifier: {0}")]
    InvalidId(#[from] uuid::Error),
}
