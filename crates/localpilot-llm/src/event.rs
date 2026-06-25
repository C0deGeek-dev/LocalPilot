//! The internal streaming event model.

use std::pin::Pin;

use futures::Stream;
use localpilot_core::TokenUsage;
use serde::{Deserialize, Serialize};

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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_metadata: Option<serde_json::Value>,
    },
    /// Token usage for the request.
    Usage(TokenUsage),
    /// A non-fatal provider warning.
    ProviderWarning { message: String },
    /// The provider stopped because the configured output limit was reached.
    /// Any streamed text before this event may be incomplete.
    OutputLimit { message: String },
    /// The stream completed normally.
    Done,
}

/// A boxed, pinned stream of model events. Boxing keeps [`ModelProvider`] object
/// safe so providers can be stored as `Box<dyn ModelProvider>`.
pub type ModelEventStream = Pin<Box<dyn Stream<Item = Result<ModelEvent, ProviderError>> + Send>>;

const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

/// Routes `<think>`-tagged inline reasoning to [`ModelEvent::ReasoningDelta`]
/// across delta boundaries.
///
/// Stateful per stream: a thinking block usually spans many deltas, and a tag
/// itself can be split across two deltas. Text that could be the start of a tag
/// is held back until the next push (or [`InlineThinkingFilter::finish`])
/// resolves it, so a partial tag at a chunk tail is never misrouted.
#[derive(Default)]
pub(crate) struct InlineThinkingFilter {
    in_thinking: bool,
    held: String,
}

impl InlineThinkingFilter {
    /// Feed one text delta; returns the events that became unambiguous.
    pub(crate) fn push(&mut self, delta: &str) -> Vec<ModelEvent> {
        self.held.push_str(delta);
        let mut events = Vec::new();
        loop {
            let (tag, make_event): (&str, fn(String) -> ModelEvent) = if self.in_thinking {
                (THINK_CLOSE, ModelEvent::ReasoningDelta)
            } else {
                (THINK_OPEN, ModelEvent::TextDelta)
            };
            if let Some(start) = self.held.find(tag) {
                let before: String = self.held[..start].to_string();
                if !before.is_empty() {
                    events.push(make_event(before));
                }
                self.held.drain(..start + tag.len());
                self.in_thinking = !self.in_thinking;
                continue;
            }
            // No complete tag: emit everything except a tail that could still
            // become one.
            let keep = partial_tag_suffix(&self.held, tag);
            let emit_len = self.held.len() - keep;
            if emit_len > 0 {
                let emitted: String = self.held.drain(..emit_len).collect();
                events.push(make_event(emitted));
            }
            return events;
        }
    }

    /// Flush held-back text at end of stream. A partial tag that never
    /// completed is plain content; an unclosed thinking block stays reasoning.
    pub(crate) fn finish(&mut self) -> Vec<ModelEvent> {
        if self.held.is_empty() {
            return Vec::new();
        }
        let text = std::mem::take(&mut self.held);
        let event = if self.in_thinking {
            ModelEvent::ReasoningDelta(text)
        } else {
            ModelEvent::TextDelta(text)
        };
        vec![event]
    }
}

/// The length of the longest proper prefix of `tag` that is a suffix of `text`.
fn partial_tag_suffix(text: &str, tag: &str) -> usize {
    (1..tag.len())
        .rev()
        .find(|&len| text.ends_with(&tag[..len]))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(deltas: &[&str]) -> (String, String) {
        let mut filter = InlineThinkingFilter::default();
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut absorb = |events: Vec<ModelEvent>| {
            for event in events {
                match event {
                    ModelEvent::TextDelta(t) => text.push_str(&t),
                    ModelEvent::ReasoningDelta(r) => reasoning.push_str(&r),
                    _ => {}
                }
            }
        };
        for delta in deltas {
            absorb(filter.push(delta));
        }
        absorb(filter.finish());
        (text, reasoning)
    }

    #[test]
    fn whole_block_in_one_delta() {
        let (text, reasoning) = run(&["answer <think>hidden</think> done"]);
        assert_eq!(text, "answer  done");
        assert_eq!(reasoning, "hidden");
    }

    #[test]
    fn block_spanning_many_deltas() {
        let (text, reasoning) = run(&[
            "<think>Let me look at",
            " the error handling",
            " here</think>",
            "The fix is simple.",
        ]);
        assert_eq!(text, "The fix is simple.");
        assert_eq!(reasoning, "Let me look at the error handling here");
    }

    #[test]
    fn open_tag_split_across_deltas() {
        let (text, reasoning) = run(&["before <thi", "nk>inside</think>after"]);
        assert_eq!(text, "before after");
        assert_eq!(reasoning, "inside");
    }

    #[test]
    fn close_tag_split_across_deltas() {
        let (text, reasoning) = run(&["<think>inside</th", "ink>after"]);
        assert_eq!(text, "after");
        assert_eq!(reasoning, "inside");
    }

    #[test]
    fn text_after_close_tag_in_same_delta() {
        let (text, reasoning) = run(&["<think>a</think>visible ", "tail"]);
        assert_eq!(text, "visible tail");
        assert_eq!(reasoning, "a");
    }

    #[test]
    fn stream_ending_inside_an_open_block_stays_reasoning() {
        let (text, reasoning) = run(&["<think>never closed", " but still hidden"]);
        assert_eq!(text, "");
        assert_eq!(reasoning, "never closed but still hidden");
    }

    #[test]
    fn lone_angle_bracket_that_is_not_a_tag_is_text() {
        let (text, reasoning) = run(&["a < b and a <t", "ag> too"]);
        assert_eq!(text, "a < b and a <tag> too");
        assert_eq!(reasoning, "");
    }

    #[test]
    fn partial_tag_at_end_of_stream_is_flushed_as_content() {
        let (text, reasoning) = run(&["trailing <thin"]);
        assert_eq!(text, "trailing <thin");
        assert_eq!(reasoning, "");
    }

    #[test]
    fn multiple_blocks_alternate_correctly() {
        let (text, reasoning) = run(&["a<think>1</think>b<think>2</think>c"]);
        assert_eq!(text, "abc");
        assert_eq!(reasoning, "12");
    }

    #[test]
    fn multibyte_text_around_tags_is_preserved() {
        let (text, reasoning) = run(&["日本語 <think>思考", "中</think> 終わり 🎉"]);
        assert_eq!(text, "日本語  終わり 🎉");
        assert_eq!(reasoning, "思考中");
    }
}
