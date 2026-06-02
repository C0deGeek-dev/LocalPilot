//! Model Context Protocol integration for Unshackled.
//!
//! Owns the MCP client: handshake, tool discovery, resource reads, and server
//! configuration/health. The defining rule is that MCP tools are not a side
//! channel — every MCP tool call is exposed as an ordinary [`Tool`] and runs
//! through the *same* permission engine and redaction pipeline as a builtin
//! tool. An MCP write prompts or is denied exactly like a builtin write.
//!
//! [`Tool`]: unshackled_tools::Tool
#![forbid(unsafe_code)]

mod client;
mod error;
mod transport;

pub use client::{McpClient, McpServerStatus, McpTool, McpToolDescriptor};
pub use error::McpError;
pub use transport::{ScriptedTransport, Transport};

/// The MCP protocol version this client speaks.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
