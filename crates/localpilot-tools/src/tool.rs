//! The tool trait, execution context, and output type.

use async_trait::async_trait;
use localpilot_sandbox::{Effect, Interactivity, Workspace};
use serde_json::Value;

use crate::error::ToolError;

/// Context passed to a tool: the workspace it may touch and how the session runs.
pub struct ToolContext<'a> {
    pub workspace: &'a Workspace,
    pub interactivity: Interactivity,
    pub trusted: bool,
    /// Where oversized tool output spills, keyed by an opaque id the model can
    /// pass to `read_tool_output`. `None` disables spilling (output is capped
    /// only).
    pub retention: Option<&'a dyn OutputRetention>,
    /// The session-scoped registry of background processes that `run_background`
    /// starts and manages. `None` disables background processes (the host wired
    /// no registry), and `run_background` reports them as unavailable.
    pub processes: Option<&'a crate::builtins_background::BackgroundProcesses>,
}

/// A tighten-only gate consulted after the permission engine for every tool
/// call. A gate can only `Pass` or `Block` — it can never grant what the
/// engine refused, so hooks extend the safety model without ever weakening
/// it. The permission engine itself is the always-on first link of this
/// chain and is not removable.
pub trait ToolGate: Send + Sync {
    /// A stable name, recorded with any block verdict.
    fn name(&self) -> &str;

    /// Inspect a call (after its effects were resolved and authorized by the
    /// engine) and either let it proceed or block it with a model-visible
    /// reason.
    fn check(&self, call: &localpilot_core::ToolCall, effects: &[Effect]) -> GateVerdict;
}

/// A gate's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    /// Let the call proceed to the next gate (or execution).
    Pass,
    /// Refuse the call with a model-visible reason.
    Block { reason: String },
}

/// A sink for full tool outputs that were too large to keep in context. The
/// host wires its store in; the registry spills, and `read_tool_output`
/// fetches.
pub trait OutputRetention: Send + Sync {
    /// Retain `output` under `id`, replacing any previous value.
    ///
    /// # Errors
    /// Returns a human-readable reason when the output cannot be retained.
    fn retain(&self, id: &str, output: &str) -> Result<(), String>;

    /// Fetch the retained output for `id`, or `None` if absent.
    ///
    /// # Errors
    /// Returns a human-readable reason when the lookup fails.
    fn fetch(&self, id: &str) -> Result<Option<String>, String>;
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

    /// A short, human-readable description of the concrete target this call
    /// acts on (the command line, path, or query), shown in approval prompts so
    /// the user sees *what* they are approving. Display-only — never an input
    /// to a permission decision. Every tool with side effects must supply one;
    /// the default empty string is acceptable only for effect-free tools.
    fn approval_detail(&self, input: &Value) -> String {
        let _ = input;
        String::new()
    }

    /// Execute the tool. Only called after every effect has been authorized.
    ///
    /// # Errors
    /// Returns [`ToolError`] on invalid input or execution failure.
    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError>;

    /// Static discipline metadata: side effects, reversibility, pre/post-
    /// conditions, and how the result is verified. Additive and advisory — the
    /// default is an empty contract, and the permission path is unaffected by it.
    fn contract(&self) -> crate::contract::ToolContract {
        crate::contract::ToolContract::default()
    }
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

/// Bound an approval-prompt detail string to a displayable length.
pub(crate) fn detail_preview(text: &str) -> String {
    const MAX_CHARS: usize = 160;
    let trimmed = text.trim();
    let mut shown: String = trimmed.chars().take(MAX_CHARS).collect();
    if trimmed.chars().count() > MAX_CHARS {
        shown.push('…');
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::SideEffectClass;
    use localpilot_sandbox::Effect;

    /// A tool that overrides nothing beyond the required methods, to prove the
    /// default contract path.
    struct NoopTool;

    #[async_trait]
    impl Tool for NoopTool {
        fn name(&self) -> &str {
            "noop"
        }
        fn description(&self) -> &str {
            ""
        }
        fn schema(&self) -> Value {
            Value::Null
        }
        fn effects(
            &self,
            _input: &Value,
            _ctx: &ToolContext<'_>,
        ) -> Result<Vec<Effect>, ToolError> {
            Ok(Vec::new())
        }
        async fn invoke(
            &self,
            _input: Value,
            _ctx: &ToolContext<'_>,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok(""))
        }
    }

    #[test]
    fn a_tool_without_an_override_reports_the_default_contract() {
        let contract = NoopTool.contract();
        assert_eq!(contract.side_effect, SideEffectClass::ReadOnly);
        assert!(contract.preconditions.is_empty());
        assert!(contract.postconditions.is_empty());
        assert!(!contract.has_side_effect());
    }
}
