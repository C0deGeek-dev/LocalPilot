//! Tool error type.

use localpilot_sandbox::SandboxError;

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

    /// Exact matching failed before any destination was written. Never use for
    /// permission refusal, commit failures or a partially applied patch.
    #[error("{0}")]
    EditPrecondition(String),

    /// The host refused prepared edits before the first write.
    #[error("{0}")]
    WorkEnvelope(String),
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
