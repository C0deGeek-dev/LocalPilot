//! Store error type.

/// Errors produced while persisting or reading local state.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// A filesystem operation failed.
    #[error("store io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// A record could not be serialized or deserialized.
    #[error("store serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// A caller-supplied key was not usable as a file name.
    #[error("invalid storage key: {0}")]
    InvalidKey(String),
}

impl StoreError {
    pub(crate) fn io(path: impl AsRef<std::path::Path>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.as_ref().display().to_string(),
            source,
        }
    }
}
