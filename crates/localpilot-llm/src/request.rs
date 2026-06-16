//! The internal request model.

use indexmap::IndexMap;
use localpilot_core::Message;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::provider::ProviderDeclaration;

/// How much reasoning/thinking effort to request from the model. Mapped per
/// provider by the adapter: a protocol shape with a documented effort field
/// uses it; a model/protocol without one clamps to a no-op — never an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
}

impl ReasoningEffort {
    /// The wire string used by effort-aware request shapes.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::Minimal => "minimal",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
        }
    }

    /// Parse a user-facing effort name.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "minimal" | "min" | "off" => Some(ReasoningEffort::Minimal),
            "low" => Some(ReasoningEffort::Low),
            "medium" | "med" => Some(ReasoningEffort::Medium),
            "high" => Some(ReasoningEffort::High),
            _ => None,
        }
    }
}

/// A provider-neutral request. Provider-specific tuning lives under
/// [`ModelRequest::options`], namespaced, reserving room for future first-class
/// fields (temperature, max output tokens, response format).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    /// Requested reasoning effort; an explicit value overrides any provider
    /// option default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// An optional JSON-schema constraint on the model's tool-call output, for a
    /// provider that declares constrained decoding. `None` for providers without
    /// the capability, so they behave exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_constraint: Option<Value>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub options: IndexMap<String, serde_json::Value>,
}

impl ModelRequest {
    /// A request with no tools and no options.
    #[must_use]
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
            reasoning_effort: None,
            tool_constraint: None,
            options: IndexMap::new(),
        }
    }

    /// Set an optional tool-call constraint (e.g. for a constrained-decoding
    /// provider). A `None` leaves the request unconstrained.
    #[must_use]
    pub fn with_tool_constraint(mut self, constraint: Option<Value>) -> Self {
        self.tool_constraint = constraint;
        self
    }

    /// Set the available tools.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.tools = tools;
        self
    }

    /// Set the requested reasoning effort.
    #[must_use]
    pub fn with_reasoning_effort(mut self, effort: Option<ReasoningEffort>) -> Self {
        self.reasoning_effort = effort;
        self
    }
}

/// A tool exposed to the model: a name, a description, and a JSON input schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// Derive a tool-call constraint from the available tools' schemas, but only for
/// a provider that declares constrained decoding. Returns `None` otherwise (and
/// when no tools are available), so a provider without the capability is
/// untouched. The constraint is a `oneOf` over `{name, arguments}` shapes, one
/// per tool, so the model must emit a schema-valid call to one of them.
#[must_use]
pub fn constraint_for(declaration: &ProviderDeclaration, tools: &[ToolSpec]) -> Option<Value> {
    if !declaration.capabilities.constrained_decoding || tools.is_empty() {
        return None;
    }
    let variants: Vec<Value> = tools
        .iter()
        .map(|tool| {
            json!({
                "type": "object",
                "properties": {
                    "name": { "const": tool.name },
                    "arguments": tool.input_schema,
                },
                "required": ["name", "arguments"],
            })
        })
        .collect();
    Some(json!({ "oneOf": variants }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{
        AuthRequirement, Capabilities, ProviderDeclaration, ReasoningShape, SourceType,
        ToolCallShape,
    };

    fn declaration(constrained_decoding: bool) -> ProviderDeclaration {
        ProviderDeclaration {
            id: "p".to_string(),
            display_name: "P".to_string(),
            source_type: SourceType::LocalServer,
            supported_input_blocks: Vec::new(),
            tool_call_shape: ToolCallShape::OpenAiToolCalls,
            reasoning_shape: ReasoningShape::None,
            capabilities: Capabilities {
                parallel_tool_calls: false,
                incremental_tool_json: false,
                reasoning: false,
                usage_during_stream: false,
                per_request_tool_disable: false,
                quota_reset_metadata: false,
                needs_no_tool_prompt_path: false,
                constrained_decoding,
            },
            max_context_tokens: None,
            auth: AuthRequirement::None,
            rate_limit_behavior: None,
        }
    }

    fn tools() -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "read_file".to_string(),
            description: "read".to_string(),
            input_schema: json!({ "type": "object", "required": ["path"] }),
        }]
    }

    #[test]
    fn a_capable_provider_gets_a_constraint_naming_the_tools() {
        let constraint = constraint_for(&declaration(true), &tools()).unwrap();
        assert!(constraint["oneOf"].is_array());
        assert_eq!(
            constraint["oneOf"][0]["properties"]["name"]["const"],
            "read_file"
        );
    }

    #[test]
    fn a_provider_without_the_capability_gets_none() {
        assert!(constraint_for(&declaration(false), &tools()).is_none());
    }

    #[test]
    fn no_tools_means_no_constraint() {
        assert!(constraint_for(&declaration(true), &[]).is_none());
    }

    #[test]
    fn a_request_carries_the_constraint_only_for_a_capable_provider() {
        let tools = tools();
        let capable = ModelRequest::new("m", Vec::new())
            .with_tools(tools.clone())
            .with_tool_constraint(constraint_for(&declaration(true), &tools));
        assert!(capable.tool_constraint.is_some());

        let hosted = ModelRequest::new("m", Vec::new())
            .with_tools(tools.clone())
            .with_tool_constraint(constraint_for(&declaration(false), &tools));
        assert!(hosted.tool_constraint.is_none());
    }
}
