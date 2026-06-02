//! MCP transport abstraction.
//!
//! A real transport speaks JSON-RPC over a server process's stdio; tests use a
//! scripted transport so the client can be exercised offline.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;

use crate::error::McpError;

/// A request/response transport to an MCP server.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Send a JSON-RPC method call and await the result value.
    ///
    /// # Errors
    /// Returns [`McpError::Transport`] or [`McpError::Protocol`] on failure.
    async fn call(&self, method: &str, params: Value) -> Result<Value, McpError>;
}

/// A scripted transport returning canned results per method, for tests.
#[derive(Default)]
pub struct ScriptedTransport {
    responses: Mutex<HashMap<String, Value>>,
}

impl ScriptedTransport {
    /// An empty scripted transport.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Script a result for a method.
    #[must_use]
    pub fn with(self, method: &str, result: Value) -> Self {
        if let Ok(mut responses) = self.responses.lock() {
            responses.insert(method.to_string(), result);
        }
        self
    }
}

#[async_trait]
impl Transport for ScriptedTransport {
    async fn call(&self, method: &str, _params: Value) -> Result<Value, McpError> {
        self.responses
            .lock()
            .ok()
            .and_then(|r| r.get(method).cloned())
            .ok_or_else(|| McpError::Protocol(format!("no scripted response for {method}")))
    }
}
