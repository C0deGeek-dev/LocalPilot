//! MCP error type.

/// Errors from the MCP client.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum McpError {
    /// The transport failed to deliver a request or response.
    #[error("mcp transport error: {0}")]
    Transport(String),

    /// The server returned a protocol error.
    #[error("mcp protocol error: {0}")]
    Protocol(String),

    /// A response could not be decoded.
    #[error("mcp decode error: {0}")]
    Decode(#[from] serde_json::Error),
}
