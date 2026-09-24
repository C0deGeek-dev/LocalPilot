//! Mesh error type.

/// Errors from reading or writing the mailbox.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MeshError {
    /// A filesystem operation failed.
    #[error("mailbox io error at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// A record exists but cannot be read, or two records contradict each
    /// other. Never read as "absent" (spec L-8).
    #[error("corrupt mailbox: {0}")]
    Corrupt(String),

    /// A record needs a protocol version or feature this build does not
    /// implement (spec V-3). Nothing was changed.
    #[error("{0}; nothing was changed")]
    Unsupported(String),

    /// A lock stayed held past its deadline (spec L-5).
    #[error("mailbox lock busy: {0}")]
    LockBusy(String),

    /// A record could not be serialized.
    #[error("mailbox serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// A storage primitive failed.
    #[error(transparent)]
    Store(#[from] localpilot_store::StoreError),
}

impl MeshError {
    pub(crate) fn io(path: &std::path::Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.display().to_string(),
            source,
        }
    }
}
