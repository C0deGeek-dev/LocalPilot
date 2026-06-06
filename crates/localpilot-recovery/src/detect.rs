//! Context-aware bad-output detection.

use serde::{Deserialize, Serialize};

/// A detected bad-output state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum BadOutputKind {
    EmptyTurn,
    RepeatedTokenLoop,
    SlashFlood,
    MalformedToolCall,
    MalformedStructuredOutput,
    RepeatedTransientError,
}

/// A run of identical punctuation outside fenced code this long is degenerate.
const SLASH_FLOOD_THRESHOLD: usize = 8;
/// Even inside fenced code, a run this long is degenerate.
const SLASH_FLOOD_IN_CODE_THRESHOLD: usize = 40;
/// A token repeated consecutively at least this many times is a loop.
const REPEATED_TOKEN_THRESHOLD: usize = 10;

/// Analyze assistant text (and whether it produced tool calls) for a bad-output
/// state. Detection is context-aware: degenerate punctuation inside fenced code
/// is tolerated until a much higher threshold.
#[must_use]
pub fn detect(text: &str, has_tool_calls: bool) -> Option<BadOutputKind> {
    if text.trim().is_empty() && !has_tool_calls {
        return Some(BadOutputKind::EmptyTurn);
    }
    if is_slash_flood(text) {
        return Some(BadOutputKind::SlashFlood);
    }
    if is_repeated_token_loop(text) {
        return Some(BadOutputKind::RepeatedTokenLoop);
    }
    None
}

/// Whether `text` contains a degenerate run of repeated punctuation, accounting
/// for fenced code blocks where such runs are common and legitimate.
#[must_use]
pub fn is_slash_flood(text: &str) -> bool {
    let (max_outside, max_inside) = max_punctuation_runs(text);
    max_outside >= SLASH_FLOOD_THRESHOLD || max_inside >= SLASH_FLOOD_IN_CODE_THRESHOLD
}

fn is_punct_run_char(c: char) -> bool {
    matches!(c, '/' | '\\' | '#' | '*' | '-' | '=' | '.' | '~')
}

/// Returns the longest run of a single repeated punctuation character outside and
/// inside fenced code blocks (delimited by ```).
fn max_punctuation_runs(text: &str) -> (usize, usize) {
    let mut in_fence = false;
    let mut max_outside = 0;
    let mut max_inside = 0;

    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        let run = longest_repeat_run(line);
        if in_fence {
            max_inside = max_inside.max(run);
        } else {
            max_outside = max_outside.max(run);
        }
    }
    (max_outside, max_inside)
}

fn longest_repeat_run(line: &str) -> usize {
    let mut best = 0;
    let mut current = 0;
    let mut previous: Option<char> = None;
    for c in line.chars() {
        if is_punct_run_char(c) && previous == Some(c) {
            current += 1;
        } else if is_punct_run_char(c) {
            current = 1;
        } else {
            current = 0;
        }
        previous = Some(c);
        best = best.max(current);
    }
    best
}

/// Whether the same whitespace-delimited token repeats consecutively past the
/// loop threshold.
#[must_use]
pub fn is_repeated_token_loop(text: &str) -> bool {
    let mut best = 0;
    let mut current = 0;
    let mut previous: Option<&str> = None;
    for token in text.split_whitespace() {
        if previous == Some(token) {
            current += 1;
        } else {
            current = 1;
        }
        previous = Some(token);
        best = best.max(current);
    }
    best >= REPEATED_TOKEN_THRESHOLD
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_turn_with_no_tool_calls_is_bad() {
        assert_eq!(detect("   ", false), Some(BadOutputKind::EmptyTurn));
        assert_eq!(detect("   ", true), None);
    }

    #[test]
    fn slash_flood_outside_code_is_detected() {
        assert_eq!(
            detect("here we go ////////////////", false),
            Some(BadOutputKind::SlashFlood)
        );
    }

    #[test]
    fn slash_like_content_inside_fenced_code_is_not_flagged() {
        let text = "Here is a path comment:\n```\n//////// not a flood, just code\n```\nok";
        assert_eq!(detect(text, false), None);
    }

    #[test]
    fn extreme_run_inside_code_still_trips_the_high_threshold() {
        let long = "/".repeat(60);
        let text = format!("```\n{long}\n```");
        assert!(is_slash_flood(&text));
    }

    #[test]
    fn repeated_token_loop_only_after_threshold() {
        let short = "na ".repeat(5);
        assert!(!is_repeated_token_loop(&short));
        let long = "na ".repeat(20);
        assert!(is_repeated_token_loop(&long));
    }
}
