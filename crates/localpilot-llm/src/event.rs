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

/// Safety valve for a `<think>` span that never closes — mirrors
/// `localx-llama`'s `ThinkStripper::THINK_BAILOUT` (same bug family: a
/// `<think>` string match can be a false positive, and nothing tells this
/// filter no closing tag is coming until the stream itself ends). Unlike
/// that stripper, `held` is drained almost every `push` call regardless of
/// `in_thinking` (see `push`'s doc comment), so this filter tracks the
/// running total in `thinking_bytes` instead of relying on `held.len()` —
/// a run-away span delivered as many small deltas (the normal case for
/// token-by-token SSE) would never trip a bare `held.len()` check.
///
/// 32 KiB, same reasoning as the LocalBox-side constant: generous for a
/// legitimate long reasoning trace (local `keep_thinking` reasoning, or
/// hosted Claude extended thinking, can run several thousand tokens), but
/// bounded well under the turn's `LocalModelMaxOutputTokens` ceiling (16384
/// tokens by default, ~60-90KB) so a misclassified span can't hold the
/// visible reply hostage for the rest of the turn. Far larger than
/// `THINK_CLOSE.len()` (8), so it cannot interact with
/// `partial_tag_suffix`'s split-tag holdback.
const THINKING_BAILOUT: usize = 32 * 1024;

/// Routes `<think>`-tagged inline reasoning to [`ModelEvent::ReasoningDelta`]
/// across delta boundaries.
///
/// Stateful per stream: a thinking block usually spans many deltas, and a tag
/// itself can be split across two deltas. Text that could be the start of a tag
/// is held back until the next push (or [`InlineThinkingFilter::finish`])
/// resolves it, so a partial tag at a chunk tail is never misrouted.
///
/// A `<think>` span that runs past `THINKING_BAILOUT` bytes without closing
/// is given up on: `push` flips back to visible-text mode and flushes
/// whatever's currently held as [`ModelEvent::TextDelta`] instead of
/// continuing to route it to [`ModelEvent::ReasoningDelta`] (the "thinking"
/// panel) for the rest of the turn.
#[derive(Default)]
pub(crate) struct InlineThinkingFilter {
    in_thinking: bool,
    held: String,
    /// Cumulative bytes classified as reasoning in the current, still-open
    /// span. Reset to 0 whenever a span (re)opens or bails out. Needed
    /// because `held` itself does not accumulate the full span — see
    /// `push`'s doc comment.
    thinking_bytes: usize,
}

impl InlineThinkingFilter {
    /// Feed one text delta; returns the events that became unambiguous.
    ///
    /// Note for the bail-out below: reasoning content is *not* held in full
    /// like `localx-llama`'s `ThinkStripper` buffers an in-think span — the
    /// "no complete tag" branch below emits eagerly (minus a small
    /// split-tag holdback) on every call, `in_thinking` or not. So `held`
    /// alone never reflects how long the current span has run;
    /// `thinking_bytes` tracks that instead.
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
                if self.in_thinking {
                    // A span just (re)opened.
                    self.thinking_bytes = 0;
                }
                continue;
            }
            if self.in_thinking
                && self.thinking_bytes.saturating_add(self.held.len()) > THINKING_BAILOUT
            {
                // This span has run past THINKING_BAILOUT bytes total
                // (across however many `push` calls it took) without a
                // `</think>` in sight. Give up treating it as reasoning:
                // flush whatever is currently held as ordinary visible text
                // and flip back to text mode so anything after streams
                // normally. Bytes already emitted as `ReasoningDelta` in
                // earlier calls for this span can't be un-sent — this stops
                // *more* of it from being misrouted, which is what unsticks
                // the visible reply going forward.
                let flushed = std::mem::take(&mut self.held);
                if !flushed.is_empty() {
                    events.push(ModelEvent::TextDelta(flushed));
                }
                self.in_thinking = false;
                self.thinking_bytes = 0;
                continue;
            }
            // No complete tag: emit everything except a tail that could still
            // become one.
            let keep = partial_tag_suffix(&self.held, tag);
            let emit_len = self.held.len() - keep;
            if emit_len > 0 {
                let emitted: String = self.held.drain(..emit_len).collect();
                if self.in_thinking {
                    self.thinking_bytes += emitted.len();
                }
                events.push(make_event(emitted));
            }
            return events;
        }
    }

    /// Flush held-back text at end of stream. A partial tag that never
    /// completed is plain content; an unclosed thinking block stays
    /// reasoning — unless it already bailed out past `THINKING_BAILOUT` in
    /// `push`, in which case `in_thinking` is already `false` here and this
    /// flushes as plain content too.
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
    fn reasoning_span_under_bailout_is_unaffected() {
        let long = "z".repeat(THINKING_BAILOUT - 1);
        let (text, reasoning) = run(&[&format!("<think>{long}")]);
        assert_eq!(text, "");
        assert_eq!(reasoning, long);
    }

    #[test]
    fn reasoning_span_over_bailout_flushes_as_text_delta_mid_stream() {
        let mut filter = InlineThinkingFilter::default();
        let long = "z".repeat(THINKING_BAILOUT + 1);
        // Flushed within this same push() call — not held for finish().
        let events = filter.push(&format!("<think>{long}"));
        assert_eq!(events, vec![ModelEvent::TextDelta(long.clone())]);

        // Later text in a subsequent push() streams as ordinary TextDelta,
        // proving `in_thinking` was reset to false, not just flushed once.
        let events2 = filter.push(" more visible text");
        assert_eq!(
            events2,
            vec![ModelEvent::TextDelta(" more visible text".to_string())]
        );
    }

    #[test]
    fn reasoning_span_delivered_as_many_small_deltas_still_trips_bailout() {
        // Realistic SSE shape: the span arrives as many small deltas, not one
        // giant one. A bailout keyed on `held.len()` alone would never fire
        // here, because `held` is drained back to near-empty on every `push`
        // regardless of `in_thinking` — this is why `thinking_bytes` tracks
        // the running total across calls instead.
        let mut filter = InlineThinkingFilter::default();
        let mut text = String::new();
        let mut reasoning = String::new();
        fn absorb(events: Vec<ModelEvent>, text: &mut String, reasoning: &mut String) {
            for event in events {
                match event {
                    ModelEvent::TextDelta(t) => text.push_str(&t),
                    ModelEvent::ReasoningDelta(r) => reasoning.push_str(&r),
                    _ => {}
                }
            }
        }
        absorb(filter.push("<think>"), &mut text, &mut reasoning);
        for _ in 0..(THINKING_BAILOUT + 100) {
            absorb(filter.push("r"), &mut text, &mut reasoning);
        }
        assert!(
            !text.is_empty(),
            "bail-out never tripped across many small deltas (reasoning={}, text={})",
            reasoning.len(),
            text.len()
        );
        absorb(
            filter.push(" more visible text"),
            &mut text,
            &mut reasoning,
        );
        assert!(text.contains("more visible text"), "text={text}");
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
