//! Provider-neutral LLM contracts.
#![forbid(unsafe_code)]

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use unshackled_core::Message;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ModelEvent {
    TextDelta(String),
    ToolCall {
        id: String,
        name: String,
        input_json: serde_json::Value,
    },
    Done,
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn stream(&self, request: ModelRequest) -> anyhow::Result<Box<dyn ModelStream>>;
}

#[async_trait]
pub trait ModelStream: Send + Unpin {
    async fn next_event(&mut self) -> anyhow::Result<Option<ModelEvent>>;
}
