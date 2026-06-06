//! Provider-neutral tool call and result model.
//!
//! These types normalize a tool invocation and its outcome independently of any
//! provider's wire format. A provider adapter translates its own representation
//! into these.

use serde::{Deserialize, Serialize};

use crate::id::ToolUseId;

/// A normalized request to run a tool, decoupled from any provider format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Correlation id matching the eventual [`ToolResult`].
    pub id: ToolUseId,
    /// The tool name as exposed to the model.
    pub name: String,
    /// The tool arguments as JSON.
    pub input: serde_json::Value,
}

impl ToolCall {
    /// Build a tool call.
    #[must_use]
    pub fn new(id: ToolUseId, name: impl Into<String>, input: serde_json::Value) -> Self {
        Self {
            id,
            name: name.into(),
            input,
        }
    }
}

/// A normalized tool outcome, correlated to a [`ToolCall`] by [`ToolCall::id`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    /// Correlation id matching the originating [`ToolCall`].
    pub id: ToolUseId,
    /// The tool's textual output.
    pub output: String,
    /// Whether the output represents a failure.
    pub is_error: bool,
}

impl ToolResult {
    /// A successful result.
    #[must_use]
    pub fn success(id: ToolUseId, output: impl Into<String>) -> Self {
        Self {
            id,
            output: output.into(),
            is_error: false,
        }
    }

    /// A failed result.
    #[must_use]
    pub fn error(id: ToolUseId, output: impl Into<String>) -> Self {
        Self {
            id,
            output: output.into(),
            is_error: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_call_roundtrips() {
        let call = ToolCall::new(
            ToolUseId::from("call_1"),
            "read_file",
            serde_json::json!({ "path": "src/lib.rs" }),
        );
        let json = serde_json::to_string(&call).unwrap();
        let back: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(call, back);
    }

    #[test]
    fn tool_result_carries_error_flag() {
        let ok = ToolResult::success(ToolUseId::from("call_1"), "done");
        let err = ToolResult::error(ToolUseId::from("call_1"), "boom");
        assert!(!ok.is_error);
        assert!(err.is_error);
        let back: ToolResult = serde_json::from_str(&serde_json::to_string(&err).unwrap()).unwrap();
        assert_eq!(err, back);
    }
}
