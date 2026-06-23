//! Provider-neutral message and content model.

use serde::{Deserialize, Serialize};

use crate::id::MessageId;
use crate::tool::{ToolCall, ToolResult};

/// Who authored a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
    /// A user-initiated shell run surfaced into the transcript. Delivered to
    /// providers as user content when included; a run marked
    /// exclude-from-context never becomes a message at all (it lives only in
    /// the session event log).
    UserShell,
}

/// A single message: an author, ordered content blocks, and optional metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "MessageMetadata::is_empty")]
    pub metadata: MessageMetadata,
}

impl Message {
    /// A message with content and no metadata.
    #[must_use]
    pub fn new(role: Role, content: Vec<ContentBlock>) -> Self {
        Self {
            role,
            content,
            metadata: MessageMetadata::default(),
        }
    }

    /// A single-text-block message.
    #[must_use]
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self::new(role, vec![ContentBlock::text(text)])
    }
}

/// Optional per-message metadata. Empty metadata is omitted on serialization.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<MessageId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Why this message was synthesized by the runtime rather than authored by
    /// the user or the model (for example a repair prompt or tool-call
    /// rejection feedback). A synthetic message still shapes the conversation
    /// the model sees and is persisted like any other, so a resumed session
    /// reconstructs exactly the history the model received.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synthetic: Option<String>,
}

impl MessageMetadata {
    fn is_empty(&self) -> bool {
        self.id.is_none() && self.provider.is_none() && self.synthetic.is_none()
    }
}

impl Message {
    /// Mark this message as runtime-synthesized, recording why.
    #[must_use]
    pub fn into_synthetic(mut self, why: impl Into<String>) -> Self {
        self.metadata.synthetic = Some(why.into());
        self
    }

    /// Whether this message was synthesized by the runtime.
    #[must_use]
    pub fn is_synthetic(&self) -> bool {
        self.metadata.synthetic.is_some()
    }
}

/// One block of message content. Growable: new block kinds may be added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ContentBlock {
    /// Plain text.
    Text { text: String },
    /// Model reasoning/thinking. `signature` and `provider_metadata` are
    /// round-tripped only when the provider requires them for tool-use
    /// continuity; the core loop treats reasoning as metadata, not final answer.
    Reasoning {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<serde_json::Value>,
    },
    /// A request to invoke a tool.
    ToolUse(ToolCall),
    /// The outcome of a tool invocation.
    ToolResult(ToolResult),
    /// An inline image attachment: base64-encoded data plus its media type
    /// (e.g. "image/png"). Sent to vision-capable providers as multimodal input.
    Image { media_type: String, data: String },
}

impl ContentBlock {
    /// A text block.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// An inline image block, carrying base64 data and its media type.
    #[must_use]
    pub fn image(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self::Image {
            media_type: media_type.into(),
            data: data.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::ToolUseId;

    fn roundtrip(block: &ContentBlock) {
        let json = serde_json::to_string(block).unwrap();
        let back: ContentBlock = serde_json::from_str(&json).unwrap();
        assert_eq!(block, &back);
    }

    #[test]
    fn every_content_block_variant_roundtrips() {
        roundtrip(&ContentBlock::text("hello"));
        roundtrip(&ContentBlock::Reasoning {
            text: "thinking".to_string(),
            signature: Some("sig".to_string()),
            provider_metadata: Some(serde_json::json!({ "k": 1 })),
        });
        roundtrip(&ContentBlock::ToolUse(ToolCall::new(
            ToolUseId::from("call_1"),
            "read_file",
            serde_json::json!({ "path": "a" }),
        )));
        roundtrip(&ContentBlock::ToolResult(ToolResult::success(
            ToolUseId::from("call_1"),
            "ok",
        )));
        roundtrip(&ContentBlock::image("image/png", "aGVsbG8="));
    }

    #[test]
    fn message_roundtrips_and_omits_empty_metadata() {
        let msg = Message::text(Role::User, "hi");
        let json = serde_json::to_string(&msg).unwrap();
        assert!(!json.contains("metadata"));
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }
}
