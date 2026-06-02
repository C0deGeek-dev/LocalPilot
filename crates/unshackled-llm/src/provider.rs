//! The provider trait, declaration, and capability descriptors.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::ProviderError;
use crate::event::ModelEventStream;
use crate::request::ModelRequest;

/// How a provider is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceType {
    /// A provider's documented official public API.
    OfficialApi,
    /// An endpoint running on the user's own machine or infrastructure.
    LocalServer,
    /// A user-configured endpoint; the user owns authorization and compliance.
    CustomUserEndpoint,
}

/// What credential a provider needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthRequirement {
    None,
    ApiKey,
}

/// A content-block kind a provider accepts as input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputBlockKind {
    Text,
    Reasoning,
    ToolResult,
}

/// The tool-call wire shape a provider uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallShape {
    /// No tool calling.
    None,
    /// OpenAI-style `tool_calls` with JSON arguments.
    OpenAiToolCalls,
}

/// The reasoning/thinking shape a provider exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningShape {
    /// No reasoning content.
    None,
    /// Reasoning content that must round-trip for tool-use continuity.
    Content,
}

/// The capability flags the session runtime branches on. The runtime must select
/// behaviour from these, never from a provider's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// The provider can run multiple tool calls in one turn.
    pub parallel_tool_calls: bool,
    /// Tool-call JSON arguments stream incrementally.
    pub incremental_tool_json: bool,
    /// Reasoning/thinking content is available.
    pub reasoning: bool,
    /// Usage arrives during streaming (vs only at completion).
    pub usage_during_stream: bool,
    /// Tools can be disabled per request.
    pub per_request_tool_disable: bool,
    /// Quota/rate-limit reset metadata is surfaced.
    pub quota_reset_metadata: bool,
    /// No-tool models need a different prompt path.
    pub needs_no_tool_prompt_path: bool,
}

/// A provider's self-description.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDeclaration {
    pub id: String,
    pub display_name: String,
    pub source_type: SourceType,
    pub supported_input_blocks: Vec<InputBlockKind>,
    pub tool_call_shape: ToolCallShape,
    pub reasoning_shape: ReasoningShape,
    pub capabilities: Capabilities,
    pub max_context_tokens: Option<u64>,
    pub auth: AuthRequirement,
    pub rate_limit_behavior: Option<String>,
}

/// A model provider. Object-safe: the streaming method returns a boxed stream so
/// providers can be held as `Box<dyn ModelProvider>`.
#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// The provider's static declaration.
    fn declaration(&self) -> &ProviderDeclaration;

    /// Start a streaming completion.
    ///
    /// # Errors
    /// Returns a [`ProviderError`] if the request cannot be started; per-event
    /// failures surface as `Err` items within the returned stream.
    async fn stream(&self, request: ModelRequest) -> Result<ModelEventStream, ProviderError>;
}
