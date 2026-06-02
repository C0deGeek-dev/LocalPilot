//! The internal streaming event model.

use std::pin::Pin;

use futures::Stream;
use serde::{Deserialize, Serialize};
use unshackled_core::TokenUsage;

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
