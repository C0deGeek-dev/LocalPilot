//! Sandbox error type.

/// Errors from workspace path resolution.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SandboxError {
    /// A candidate path resolves outside the workspace root.
    #[error("path escapes the workspace boundary: {path}")]
    OutsideWorkspace { path: String },

    /// A filesystem operation failed while resolving a path.
    #[error("could not resolve path {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}
