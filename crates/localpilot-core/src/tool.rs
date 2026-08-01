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
    /// Provider-specific metadata that must round-trip with the tool call.
    ///
    /// For example, Gemini's OpenAI-compatible endpoint attaches
    /// `extra_content.google.thought_signature` to tool calls and requires the
    /// exact value to be returned with the assistant tool-call message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<serde_json::Value>,
}

impl ToolCall {
    /// Build a tool call.
    #[must_use]
    pub fn new(id: ToolUseId, name: impl Into<String>, input: serde_json::Value) -> Self {
        Self {
            id,
            name: name.into(),
            input,
            provider_metadata: None,
        }
    }

    /// Attach provider-specific metadata that should be preserved verbatim.
    #[must_use]
    pub fn with_provider_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.provider_metadata = Some(metadata);
        self
    }
}

/// How a tool call turned out. Three states, because the two failure kinds are
/// materially different: a tool that ran to completion whose wrapped work said
/// no is information the model must act on, while a tool that could not do its
/// job at all teaches nothing about the underlying work. Collapsing them into
/// one boolean makes every consumer re-derive a distinction the type should
/// carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    /// The tool ran and the work it wrapped succeeded.
    Ok,
    /// The tool ran to completion and the work it wrapped reported failure:
    /// a non-zero exit, an error HTTP status, a background process that died
    /// inside its grace period. The tool is healthy; the world said no.
    ReportedFailure,
    /// The tool could not do its job at all: a spawn error, a timeout, invalid
    /// input, an unknown tool, an effects error, a permission denial, a gate
    /// block, a cancellation.
    Unusable,
}

impl ToolOutcome {
    /// Whether the model sees this result as an error (`status: error`).
    #[must_use]
    pub fn is_error(self) -> bool {
        !matches!(self, Self::Ok)
    }

    /// Whether the tool itself malfunctioned — the property tool-health guards
    /// measure. A reported failure is direct evidence *against* malfunction:
    /// the tool ran and captured its result.
    #[must_use]
    pub fn is_malfunction(self) -> bool {
        matches!(self, Self::Unusable)
    }

    /// The status word rendered into model-visible result text.
    #[must_use]
    pub fn status_label(self) -> &'static str {
        if self.is_error() {
            "error"
        } else {
            "success"
        }
    }
}

/// A normalized tool outcome, correlated to a [`ToolCall`] by [`ToolCall::id`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "ToolResultWire", into = "ToolResultWire")]
pub struct ToolResult {
    /// Correlation id matching the originating [`ToolCall`].
    pub id: ToolUseId,
    /// The tool's textual output.
    pub output: String,
    /// How the call turned out.
    pub outcome: ToolOutcome,
}

/// The wire shape of [`ToolResult`] — a strict superset of the pre-outcome
/// format. `is_error` is always written so old readers keep working, and
/// `outcome` is an optional refinement so lines written before it existed
/// still parse. This shim is load-bearing, not cosmetic: transcript reads drop
/// unparseable lines silently, so a breaking change to this shape would lose
/// session history rather than error.
#[derive(Serialize, Deserialize)]
struct ToolResultWire {
    id: ToolUseId,
    output: String,
    is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    outcome: Option<ToolOutcome>,
}

impl From<ToolResultWire> for ToolResult {
    fn from(wire: ToolResultWire) -> Self {
        let fallback = if wire.is_error {
            ToolOutcome::Unusable
        } else {
            ToolOutcome::Ok
        };
        Self {
            id: wire.id,
            output: wire.output,
            outcome: wire.outcome.unwrap_or(fallback),
        }
    }
}

impl From<ToolResult> for ToolResultWire {
    fn from(result: ToolResult) -> Self {
        Self {
            id: result.id,
            output: result.output,
            is_error: result.outcome.is_error(),
            outcome: Some(result.outcome),
        }
    }
}

impl ToolResult {
    /// A successful result.
    #[must_use]
    pub fn success(id: ToolUseId, output: impl Into<String>) -> Self {
        Self {
            id,
            output: output.into(),
            outcome: ToolOutcome::Ok,
        }
    }

    /// A failed result: the tool could not do its job at all.
    #[must_use]
    pub fn error(id: ToolUseId, output: impl Into<String>) -> Self {
        Self {
            id,
            output: output.into(),
            outcome: ToolOutcome::Unusable,
        }
    }

    /// A result whose tool ran fine and whose wrapped work reported failure.
    #[must_use]
    pub fn reported_failure(id: ToolUseId, output: impl Into<String>) -> Self {
        Self {
            id,
            output: output.into(),
            outcome: ToolOutcome::ReportedFailure,
        }
    }

    /// Whether the model sees this result as an error.
    #[must_use]
    pub fn is_error(&self) -> bool {
        self.outcome.is_error()
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
    fn tool_result_carries_outcome() {
        let ok = ToolResult::success(ToolUseId::from("call_1"), "done");
        let err = ToolResult::error(ToolUseId::from("call_1"), "boom");
        let reported = ToolResult::reported_failure(ToolUseId::from("call_1"), "exit: 1");
        assert!(!ok.is_error());
        assert!(err.is_error());
        assert!(err.outcome.is_malfunction());
        assert!(reported.is_error());
        assert!(!reported.outcome.is_malfunction());
        let back: ToolResult = serde_json::from_str(&serde_json::to_string(&err).unwrap()).unwrap();
        assert_eq!(err, back);
    }

    #[test]
    fn pre_outcome_transcript_line_still_parses() {
        // A line written before `outcome` existed must not be dropped by the
        // transcript reader; the boolean degrades to its historical meaning.
        let old_error: ToolResult =
            serde_json::from_str(r#"{"id":"c1","output":"boom","is_error":true}"#).unwrap();
        assert_eq!(old_error.outcome, ToolOutcome::Unusable);
        let old_ok: ToolResult =
            serde_json::from_str(r#"{"id":"c1","output":"done","is_error":false}"#).unwrap();
        assert_eq!(old_ok.outcome, ToolOutcome::Ok);
    }

    #[test]
    fn reported_failure_serializes_as_superset_and_round_trips() {
        let reported = ToolResult::reported_failure(ToolUseId::from("c1"), "exit: 3");
        let json = serde_json::to_string(&reported).unwrap();
        // Old readers key on the boolean; new readers refine it.
        assert!(json.contains(r#""is_error":true"#));
        assert!(json.contains(r#""outcome":"reported_failure""#));
        let back: ToolResult = serde_json::from_str(&json).unwrap();
        assert_eq!(reported, back);
    }
}
