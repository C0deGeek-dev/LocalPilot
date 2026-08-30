//! Store error type.

use localpilot_core::SessionId;

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

    /// A session name was empty, or looked like a session id (a name that parses
    /// as a UUID would be ambiguous with an id when resuming).
    #[error("invalid session name: {0}")]
    InvalidName(String),

    /// The requested session name is already held by a different session in this
    /// workspace; names are unique so a name always resolves to one session.
    #[error("session name {name:?} is already used by session {existing}")]
    NameTaken { name: String, existing: SessionId },

    /// A record was written by a format version this build cannot read.
    #[error("unsupported record format version {found} (this build reads up to {supported})")]
    UnsupportedFormat { found: u64, supported: u32 },

    /// The per-user base directory for the global prompt-history store could not
    /// be resolved (no `APPDATA`/`XDG_CONFIG_HOME`/`HOME` set).
    #[error("could not resolve a per-user directory for the prompt-history store")]
    NoUserDir,
}

impl StoreError {
    pub(crate) fn io(path: impl AsRef<std::path::Path>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.as_ref().display().to_string(),
            source,
        }
    }
}
