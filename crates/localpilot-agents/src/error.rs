//! Errors from loading and resolving subagent definitions.

/// An error from the definition layer.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AgentError {
    /// The file could not be read.
    #[error("read definition: {0}")]
    Io(String),

    /// The YAML was malformed, or carried a field this build does not know.
    #[error("parse definition: {0}")]
    Parse(String),

    /// The definition parsed but broke a validation rule.
    #[error("invalid definition: {0}")]
    Invalid(String),

    /// A tool entry named a tool that is not registered.
    #[error("unknown tool: {0}")]
    UnknownTool(String),
}
