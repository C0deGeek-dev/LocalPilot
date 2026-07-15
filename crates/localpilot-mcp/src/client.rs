//! The MCP protocol client and the `Tool` adapter.

use std::sync::Arc;

use async_trait::async_trait;
use localpilot_sandbox::Effect;
use localpilot_tools::{Tool, ToolContext, ToolError, ToolOutput};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::McpError;
use crate::transport::Transport;
use crate::MCP_PROTOCOL_VERSION;

/// The client name reported to servers in the initialize handshake.
const CLIENT_NAME: &str = "localpilot";
/// The client version reported to servers in the initialize handshake.
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// A tool advertised by an MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpToolDescriptor {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "inputSchema")]
    pub input_schema: Value,
}

/// The status of a connected MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerStatus {
    pub connected: bool,
    pub protocol_version: String,
}

/// An MCP protocol client over a transport.
pub struct McpClient {
    transport: Arc<dyn Transport>,
}

impl McpClient {
    /// Build a client over `transport`.
    #[must_use]
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self { transport }
    }

    /// Perform the initialize handshake.
    ///
    /// # Errors
    /// Returns [`McpError`] if the transport or response is invalid.
    pub async fn initialize(&self) -> Result<McpServerStatus, McpError> {
        // The MCP spec requires `protocolVersion`, `capabilities`, and
        // `clientInfo`; strict servers reject the handshake when the latter two
        // are absent. We advertise no special client capabilities (`{}`).
        let result = self
            .transport
            .call(
                "initialize",
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": CLIENT_NAME,
                        "version": CLIENT_VERSION,
                    },
                }),
            )
            .await?;
        let protocol_version = result["protocolVersion"]
            .as_str()
            .unwrap_or(MCP_PROTOCOL_VERSION)
            .to_string();
        // The spec requires the client to acknowledge a successful initialize
        // with an `initialized` notification before issuing further requests.
        self.transport
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(McpServerStatus {
            connected: true,
            protocol_version,
        })
    }

    /// Discover the server's tools.
    ///
    /// # Errors
    /// Returns [`McpError`] if the transport or response is invalid.
    pub async fn list_tools(&self) -> Result<Vec<McpToolDescriptor>, McpError> {
        let result = self.transport.call("tools/list", json!({})).await?;
        let tools = result["tools"].clone();
        Ok(serde_json::from_value(tools).unwrap_or_default())
    }

    /// Call a tool and return its textual content.
    ///
    /// # Errors
    /// Returns [`McpError`] if the transport or response is invalid.
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<String, McpError> {
        let result = self.call_tool_raw(name, arguments).await?;
        Ok(extract_text(&result))
    }

    /// Call a tool and return the full result value — `content` items,
    /// `structuredContent`, and `isError` intact — for callers that need more
    /// than flattened text (e.g. search-result parsing).
    ///
    /// # Errors
    /// Returns [`McpError`] if the transport or response is invalid.
    pub async fn call_tool_raw(&self, name: &str, arguments: Value) -> Result<Value, McpError> {
        self.transport
            .call(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await
    }

    /// Read a resource and return its textual content.
    ///
    /// # Errors
    /// Returns [`McpError`] if the transport or response is invalid.
    pub async fn read_resource(&self, uri: &str) -> Result<String, McpError> {
        let result = self
            .transport
            .call("resources/read", json!({ "uri": uri }))
            .await?;
        Ok(extract_text(&result))
    }
}

fn extract_text(result: &Value) -> String {
    if let Some(items) = result["content"].as_array() {
        items
            .iter()
            .filter_map(|item| item["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        result.to_string()
    }
}

/// An MCP tool exposed as a builtin [`Tool`]. It declares its effects so the
/// permission engine gates it exactly like a builtin tool, and its output is
/// redacted by the same registry dispatch — MCP is never a side channel.
pub struct McpTool {
    name: String,
    remote_name: String,
    description: String,
    schema: Value,
    effects: Vec<Effect>,
    transport: Arc<dyn Transport>,
}

impl McpTool {
    /// Wrap an MCP tool with the effects it should be gated on.
    #[must_use]
    pub fn new(
        descriptor: &McpToolDescriptor,
        effects: Vec<Effect>,
        transport: Arc<dyn Transport>,
    ) -> Self {
        Self {
            name: descriptor.name.clone(),
            remote_name: descriptor.name.clone(),
            description: if descriptor.description.is_empty() {
                "MCP tool".to_string()
            } else {
                descriptor.description.clone()
            },
            schema: descriptor.input_schema.clone(),
            effects,
            transport,
        }
    }

    /// Override the name exposed to the model without changing the name sent
    /// back to the MCP server.
    #[must_use]
    pub fn advertised_as(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        self.schema.clone()
    }

    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(self.effects.clone())
    }

    async fn invoke(&self, input: Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let client = McpClient::new(Arc::clone(&self.transport));
        let text = client
            .call_tool(&self.remote_name, input)
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        Ok(ToolOutput::ok(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::ScriptedTransport;

    #[tokio::test]
    async fn handshake_and_tool_discovery() {
        let transport = Arc::new(
            ScriptedTransport::new()
                .with("initialize", json!({ "protocolVersion": MCP_PROTOCOL_VERSION }))
                .with(
                    "tools/list",
                    json!({ "tools": [
                        { "name": "echo", "description": "echo text", "inputSchema": { "type": "object" } }
                    ] }),
                ),
        );
        let client = McpClient::new(transport);

        let status = client.initialize().await.unwrap();
        assert!(status.connected);
        assert_eq!(status.protocol_version, MCP_PROTOCOL_VERSION);

        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
    }

    #[tokio::test]
    async fn call_tool_extracts_text_content() {
        let transport = Arc::new(ScriptedTransport::new().with(
            "tools/call",
            json!({ "content": [{ "type": "text", "text": "hello from mcp" }] }),
        ));
        let client = McpClient::new(transport);
        let out = client
            .call_tool("echo", json!({ "text": "hi" }))
            .await
            .unwrap();
        assert_eq!(out, "hello from mcp");
    }

    #[tokio::test]
    async fn renamed_tool_advertises_alias_but_calls_remote_name() {
        let transport = Arc::new(ScriptedTransport::new().with(
            "tools/call",
            json!({ "content": [{ "type": "text", "text": "remote result" }] }),
        ));
        let descriptor = McpToolDescriptor {
            name: "fetch".to_string(),
            description: "fetch a remote resource".to_string(),
            input_schema: json!({ "type": "object" }),
        };
        let tool = McpTool::new(&descriptor, vec![Effect::Network], transport.clone())
            .advertised_as("duckduckgo_fetch");

        assert_eq!(tool.name(), "duckduckgo_fetch");

        let dir = tempfile::tempdir().unwrap();
        let workspace = localpilot_sandbox::Workspace::new(dir.path()).unwrap();
        let context = ToolContext {
            workspace: &workspace,
            interactivity: localpilot_sandbox::Interactivity::NonInteractive,
            trusted: true,
            retention: None,
            processes: None,
        };
        let input = json!({ "url": "https://example.test" });
        let output = tool.invoke(input.clone(), &context).await.unwrap();

        assert_eq!(output.text, "remote result");
        assert_eq!(
            transport.calls(),
            vec![(
                "tools/call".to_string(),
                json!({ "name": "fetch", "arguments": input }),
            )]
        );
    }
}
