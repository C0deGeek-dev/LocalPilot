//! The internal request model.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use unshackled_core::Message;

/// A provider-neutral request. Provider-specific tuning lives under
/// [`ModelRequest::options`], namespaced, reserving room for future first-class
/// fields (temperature, max output tokens, reasoning effort, response format).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
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
            options: IndexMap::new(),
        }
    }

    /// Set the available tools.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.tools = tools;
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
