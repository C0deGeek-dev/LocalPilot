//! The internal streaming event model.

use std::pin::Pin;

use futures::Stream;
use serde::{Deserialize, Serialize};
use localpilot_core::TokenUsage;

use crate::error::ProviderError;

/// One event in a provider response stream. Growable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ModelEvent {
    /// A chunk of final-answer text.
    TextDelta(String),
    /// A chunk of reasoning/thinking content. Display-only metadata; never the
    /// final answer.
    ReasoningDelta(String),
    /// A fully assembled tool call. The adapter accumulates any incremental
    /// argument fragments before emitting this.
    ToolCall {
        id: String,
        name: String,
        input_json: serde_json::Value,
    },
    /// Token usage for the request.
    Usage(TokenUsage),
    /// A non-fatal provider warning.
    ProviderWarning { message: String },
    /// The stream completed normally.
    Done,
}

/// A boxed, pinned stream of model events. Boxing keeps [`ModelProvider`] object
/// safe so providers can be stored as `Box<dyn ModelProvider>`.
pub type ModelEventStream = Pin<Box<dyn Stream<Item = Result<ModelEvent, ProviderError>> + Send>>;

pub(crate) fn split_inline_thinking(text: &str) -> Vec<ModelEvent> {
    let mut events = Vec::new();
    let mut rest = text;
    loop {
        let Some(start) = rest.find("<think>") else {
            if !rest.is_empty() {
                events.push(ModelEvent::TextDelta(rest.to_string()));
            }
            break;
        };
        let before = &rest[..start];
        if !before.is_empty() {
            events.push(ModelEvent::TextDelta(before.to_string()));
        }
        let after_start = &rest[start + "<think>".len()..];
        let Some(end) = after_start.find("</think>") else {
            events.push(ModelEvent::ReasoningDelta(after_start.to_string()));
            break;
        };
        let thinking = &after_start[..end];
        if !thinking.is_empty() {
            events.push(ModelEvent::ReasoningDelta(thinking.to_string()));
        }
        rest = &after_start[end + "</think>".len()..];
    }
    events
}
