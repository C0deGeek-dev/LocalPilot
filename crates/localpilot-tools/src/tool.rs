//! The tool trait, execution context, and output type.

use async_trait::async_trait;
use serde_json::Value;
use localpilot_sandbox::{Effect, Interactivity, Workspace};

use crate::error::ToolError;

/// Context passed to a tool: the workspace it may touch and how the session runs.
pub struct ToolContext<'a> {
    pub workspace: &'a Workspace,
    pub interactivity: Interactivity,
    pub trusted: bool,
}

/// A tool's textual result, before redaction and the final id are attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
    pub truncated: bool,
}

impl ToolOutput {
    /// A successful output.
    #[must_use]
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
            truncated: false,
        }
    }

    /// A successful output marked as truncated.
    #[must_use]
    pub fn truncated(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
            truncated: true,
        }
    }
}

/// A builtin tool. Object-safe so the registry can hold `Box<dyn Tool>`.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The tool's stable name as exposed to the model.
    fn name(&self) -> &str;

    /// A one-line description.
    fn description(&self) -> &str;

    /// The JSON schema for this tool's input, generated from a typed struct.
    fn schema(&self) -> Value;

    /// The side effects this call will have, used to drive the permission engine.
    /// Resolving effects must not itself perform the effect.
    ///
    /// # Errors
    /// Returns [`ToolError::InvalidInput`] if the input does not parse.
    fn effects(&self, input: &Value, ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError>;

    /// Execute the tool. Only called after every effect has been authorized.
    ///
    /// # Errors
    /// Returns [`ToolError`] on invalid input or execution failure.
    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError>;
}

/// Parse a tool's JSON input into a typed struct.
///
/// # Errors
/// Returns [`ToolError::InvalidInput`] if deserialization fails.
pub(crate) fn parse_input<T: serde::de::DeserializeOwned>(input: &Value) -> Result<T, ToolError> {
    serde_json::from_value(input.clone()).map_err(|e| ToolError::InvalidInput(e.to_string()))
}

/// Generate a JSON schema value from a typed input struct.
pub(crate) fn schema_for<T: schemars::JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null)
}
