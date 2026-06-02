//! Tool error type.

use unshackled_sandbox::SandboxError;

/// Errors from preparing or executing a tool. A failed tool call is surfaced to
/// the model as data (an error [`crate::ToolResult`]), never as a process crash.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ToolError {
    /// The requested tool is not registered.
    #[error("unknown tool: {0}")]
    UnknownTool(String),

    /// The tool input did not match the schema.
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// The effect was denied by the permission engine or the user.
    #[error("permission denied: {0}")]
    PermissionDenied(String),

    /// A path escaped the workspace boundary.
    #[error("path is outside the workspace: {0}")]
    OutsideWorkspace(String),

    /// A filesystem or process error.
    #[error("{0}")]
    Failed(String),
}

impl From<SandboxError> for ToolError {
    fn from(err: SandboxError) -> Self {
        match err {
            SandboxError::OutsideWorkspace { path } => ToolError::OutsideWorkspace(path),
            SandboxError::Io { path, source } => ToolError::Failed(format!("{path}: {source}")),
            other => ToolError::Failed(other.to_string()),
        }
    }
}
