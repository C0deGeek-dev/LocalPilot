//! Core domain types for Unshackled.
//!
//! This crate must stay provider-neutral and UI-neutral.
#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
        signature: Option<String>,
        provider_metadata: Option<serde_json::Value>,
    },
    ToolUse {
        id: String,
        name: String,
        input_json: serde_json::Value,
    },
    ToolResult {
        id: String,
        output: String,
        is_error: bool,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum UnshackledError {
    #[error("{0}")]
    Message(String),
}
