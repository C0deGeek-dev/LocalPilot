//! Context-aware bad-output detection.

use std::collections::HashSet;

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
    /// The same tool call failed, then was repeated unchanged — a futile loop
    /// distinct from output degeneracy.
    ToolCallLoop,
}

/// A run of forward slashes outside fenced code this long is degenerate.
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

/// Whether `text` contains a degenerate run of repeated slashes, accounting
/// for fenced code blocks where such runs are common and legitimate.
#[must_use]
pub fn is_slash_flood(text: &str) -> bool {
    let (max_outside, max_inside) = max_punctuation_runs(text);
    max_outside >= SLASH_FLOOD_THRESHOLD || max_inside >= SLASH_FLOOD_IN_CODE_THRESHOLD
}

fn is_punct_run_char(c: char) -> bool {
    c == '/'
}

/// Returns the longest run of forward slashes outside and inside fenced code
/// blocks (delimited by ```).
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

/// Incremental degenerate-output monitor for live streams.
///
/// Produces the same verdict as [`is_slash_flood`] `||`
/// [`is_repeated_token_loop`] over the accumulated text, but in O(delta) work
/// per pushed chunk instead of rescanning the whole turn — the live guard runs
/// on every delta of a potentially unbounded stream.
#[derive(Debug, Default)]
pub struct StreamMonitor {
    in_fence: bool,
    line_lead: LineLead,
    prev_char: Option<char>,
    run: usize,
    line_max_run: usize,
    max_outside: usize,
    max_inside: usize,
    prev_token: String,
    token: String,
    repeat: usize,
    max_repeat: usize,
}

/// Whether the current line's leading non-whitespace begins a ``` fence marker.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum LineLead {
    /// Still reading leading whitespace / backticks.
    #[default]
    Pending,
    /// The line starts with ``` (a fence-toggle line; its runs do not count).
    Fence,
    /// The line starts with ordinary content.
    Content,
}

impl StreamMonitor {
    /// Feed one stream delta.
    pub fn push(&mut self, delta: &str) {
        let mut backticks_pending = 0u8;
        for c in delta.chars() {
            if c == '\n' {
                self.close_line();
                backticks_pending = 0;
                continue;
            }
            // Decide whether this line is a fence-toggle line from its leading
            // non-whitespace characters (mirrors `trim_start().starts_with("```")`).
            if self.line_lead == LineLead::Pending {
                if c == '`' {
                    backticks_pending += 1;
                    if backticks_pending == 3 {
                        self.line_lead = LineLead::Fence;
                    }
                } else if c.is_whitespace() && backticks_pending == 0 {
                    // still in leading whitespace
                } else {
                    self.line_lead = LineLead::Content;
                }
            }
            // Punctuation-run tracking (a fence-toggle line's runs are excluded).
            if self.line_lead != LineLead::Fence {
                if is_punct_run_char(c) && self.prev_char == Some(c) {
                    self.run += 1;
                } else if is_punct_run_char(c) {
                    self.run = 1;
                } else {
                    self.run = 0;
                }
                self.line_max_run = self.line_max_run.max(self.run);
            }
            self.prev_char = Some(c);
            // Token-loop tracking.
            if c.is_whitespace() {
                self.close_token();
            } else {
                self.token.push(c);
            }
        }
    }

    fn close_line(&mut self) {
        if self.line_lead == LineLead::Fence {
            self.in_fence = !self.in_fence;
        } else if self.in_fence {
            self.max_inside = self.max_inside.max(self.line_max_run);
        } else {
            self.max_outside = self.max_outside.max(self.line_max_run);
        }
        self.line_lead = LineLead::Pending;
        self.prev_char = None;
        self.run = 0;
        self.line_max_run = 0;
        self.close_token();
    }

    fn close_token(&mut self) {
        if self.token.is_empty() {
            return;
        }
        if self.token == self.prev_token {
            self.repeat += 1;
        } else {
            self.repeat = 1;
        }
        self.max_repeat = self.max_repeat.max(self.repeat);
        std::mem::swap(&mut self.prev_token, &mut self.token);
        self.token.clear();
    }

    /// Whether the accumulated stream is degenerate (punctuation flood or
    /// repeated-token loop), including the still-open line and token.
    #[must_use]
    pub fn detected(&self) -> bool {
        let (mut outside, mut inside) = (self.max_outside, self.max_inside);
        if self.line_lead != LineLead::Fence {
            if self.in_fence {
                inside = inside.max(self.line_max_run);
            } else {
                outside = outside.max(self.line_max_run);
            }
        }
        let repeat = if self.token.is_empty() {
            self.max_repeat
        } else if self.token == self.prev_token {
            self.max_repeat.max(self.repeat + 1)
        } else {
            self.max_repeat.max(1)
        };
        outside >= SLASH_FLOOD_THRESHOLD
            || inside >= SLASH_FLOOD_IN_CODE_THRESHOLD
            || repeat >= REPEATED_TOKEN_THRESHOLD
    }
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

/// Detects a futile tool-call loop within a step: the same failing call
/// repeated. It escalates on the **second** occurrence of a failing call
/// signature (a fast break, before the per-tool failure safeguard's higher
/// threshold). A successful call does not seed the loop, so a corrected retry
/// with different arguments never trips it.
///
/// The caller supplies an opaque signature per call (e.g. `name` + arguments);
/// this detector stays free of any tool or JSON dependency.
#[derive(Debug, Default)]
pub struct ToolLoopDetector {
    failing_signatures: HashSet<String>,
}

impl ToolLoopDetector {
    /// A fresh detector for a step.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one tool call's outcome. Returns `true` the moment a *failing*
    /// call repeats a failing-call signature already seen this step — the point
    /// to escalate rather than loop.
    pub fn observe(&mut self, signature: &str, failed: bool) -> bool {
        // A success is not part of a futile loop and never seeds one.
        failed && !self.failing_signatures.insert(signature.to_string())
    }
}

/// Batch reference for [`ToolLoopDetector`]: whether any failing-call signature
/// repeats over the whole sequence. Used to prove the incremental detector.
#[must_use]
pub fn has_tool_loop<'a, I>(calls: I) -> bool
where
    I: IntoIterator<Item = (&'a str, bool)>,
{
    let mut seen = HashSet::new();
    for (signature, failed) in calls {
        if failed && !seen.insert(signature) {
            return true;
        }
    }
    false
}

/// Default number of consecutive identical tool errors that trips the
/// same-error breaker. Kept below the per-tool failure budget so a strategy
/// change is forced *before* the budget is exhausted.
pub const SAME_ERROR_THRESHOLD: usize = 3;

/// Normalize a tool error into a signature so cosmetically-different repeats of
/// the *same* failure compare equal: lowercased, with internal whitespace runs
/// collapsed and the ends trimmed. Keys on the error class, not exact bytes.
#[must_use]
pub fn error_signature(error: &str) -> String {
    error
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

/// Detects a tool failing repeatedly with the *same* error within a turn — a
/// futile loop the model should break by changing approach (write a script file,
/// confirm a tool exists), distinct from the call-signature loop
/// [`ToolLoopDetector`] catches. It fires when the same `(tool, normalized error)`
/// signature recurs `threshold` times in a row, and only on that crossing, so a
/// sustained loop surfaces one strategy-change hint rather than a flood.
#[derive(Debug, Clone)]
pub struct RepeatedErrorBreaker {
    threshold: usize,
    last: Option<String>,
    streak: usize,
}

impl Default for RepeatedErrorBreaker {
    fn default() -> Self {
        Self::new(SAME_ERROR_THRESHOLD)
    }
}

impl RepeatedErrorBreaker {
    /// A breaker that fires after `threshold` consecutive identical errors
    /// (clamped to at least 1).
    #[must_use]
    pub fn new(threshold: usize) -> Self {
        Self {
            threshold: threshold.max(1),
            last: None,
            streak: 0,
        }
    }

    /// Record one tool *failure*. Returns `true` exactly when the consecutive
    /// streak of an identical `(tool, error)` signature reaches the threshold —
    /// the moment to force a strategy change. A different signature restarts the
    /// streak, so distinct errors never trip it.
    pub fn observe(&mut self, tool: &str, error: &str) -> bool {
        // The unit separator cannot appear in a tool name, so it cleanly
        // delimits tool from error in the combined signature.
        let signature = format!("{tool}\u{1f}{}", error_signature(error));
        if self.last.as_deref() == Some(signature.as_str()) {
            self.streak += 1;
        } else {
            self.last = Some(signature);
            self.streak = 1;
        }
        self.streak == self.threshold
    }

    /// Reset after a successful call or at a turn boundary, so progress (or a new
    /// turn) clears the streak and only genuinely consecutive failures count.
    pub fn reset(&mut self) {
        self.last = None;
        self.streak = 0;
    }
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
    fn markdown_table_separator_is_not_a_slash_flood() {
        let table =
            "| Project | Test Coverage | Assessment |\n|---------|---------------|------------|";
        assert_eq!(detect(table, false), None);

        let mut monitor = StreamMonitor::default();
        for chunk in table.as_bytes().chunks(7) {
            monitor.push(std::str::from_utf8(chunk).expect("ascii table"));
        }
        assert!(!monitor.detected());
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

    #[test]
    fn stream_monitor_matches_the_full_scan_on_representative_streams() {
        let cases = [
            "here we go ////////////////",
            "Here is a path comment:\n```\n//////// not a flood, just code\n```\nok",
            &"/".repeat(60),
            &format!("```\n{}\n```", "/".repeat(60)),
            &"na ".repeat(20),
            &"na ".repeat(5),
            "normal prose with no degeneration at all",
            "  ```rust\n====== separator ======\n```",
            "===== eight ========\ntext",
        ];
        for text in cases {
            let mut monitor = StreamMonitor::default();
            monitor.push(text);
            let expected = is_slash_flood(text) || is_repeated_token_loop(text);
            assert_eq!(monitor.detected(), expected, "text: {text:?}");
        }
    }

    proptest::proptest! {
        // The incremental monitor agrees with the full rescan regardless of how
        // the stream is chunked.
        #[test]
        fn stream_monitor_is_equivalent_to_full_rescan(
            pieces in proptest::collection::vec("[a-z/=#.\\-`\\n ]{0,12}", 0..24)
        ) {
            let text: String = pieces.concat();
            let mut monitor = StreamMonitor::default();
            for piece in &pieces {
                monitor.push(piece);
            }
            let expected = is_slash_flood(&text) || is_repeated_token_loop(&text);
            proptest::prop_assert_eq!(monitor.detected(), expected);
        }
    }

    #[test]
    fn two_identical_failing_calls_escalate() {
        let mut detector = ToolLoopDetector::new();
        let sig = "edit_file|{\"path\":\"a.rs\",\"old\":\"x\"}";
        assert!(
            !detector.observe(sig, true),
            "first failure is not yet a loop"
        );
        assert!(
            detector.observe(sig, true),
            "the identical repeat of a failing call escalates"
        );
    }

    #[test]
    fn a_corrected_retry_does_not_trip_the_detector() {
        let mut detector = ToolLoopDetector::new();
        assert!(!detector.observe("edit_file|{\"old\":\"x\"}", true));
        // A different (corrected) call, even if it also fails, is novel.
        assert!(!detector.observe("edit_file|{\"old\":\"y\"}", true));
        // And a success never seeds a loop.
        assert!(!detector.observe("edit_file|{\"old\":\"z\"}", false));
    }

    #[test]
    fn same_error_breaker_fires_after_three_identical_errors() {
        let mut breaker = RepeatedErrorBreaker::new(SAME_ERROR_THRESHOLD);
        let err = "no closing quotation mark";
        assert!(!breaker.observe("run_shell", err), "1st identical error");
        assert!(!breaker.observe("run_shell", err), "2nd identical error");
        assert!(
            breaker.observe("run_shell", err),
            "3rd identical error trips it"
        );
        // It fires once on the crossing, not on every later repeat.
        assert!(
            !breaker.observe("run_shell", err),
            "no re-fire while still stuck"
        );
    }

    #[test]
    fn same_error_breaker_normalizes_cosmetic_differences() {
        let mut breaker = RepeatedErrorBreaker::new(2);
        assert!(!breaker.observe("run_shell", "No closing  quote"));
        // Different whitespace/case is the same error class → trips on the repeat.
        assert!(breaker.observe("run_shell", "no closing quote"));
    }

    #[test]
    fn distinct_errors_do_not_trip_the_breaker() {
        let mut breaker = RepeatedErrorBreaker::new(SAME_ERROR_THRESHOLD);
        assert!(!breaker.observe("run_shell", "missing quote"));
        assert!(!breaker.observe("run_shell", "file not found"));
        assert!(!breaker.observe("run_shell", "permission denied"));
        // A different tool with the same text is a different signature, too.
        let mut breaker = RepeatedErrorBreaker::new(2);
        assert!(!breaker.observe("run_shell", "boom"));
        assert!(!breaker.observe("edit_file", "boom"));
    }

    #[test]
    fn a_success_resets_the_breaker_streak() {
        let mut breaker = RepeatedErrorBreaker::new(SAME_ERROR_THRESHOLD);
        let err = "same error";
        assert!(!breaker.observe("run_shell", err));
        assert!(!breaker.observe("run_shell", err));
        breaker.reset(); // a clean call landed in between
        assert!(
            !breaker.observe("run_shell", err),
            "streak restarts after reset"
        );
        assert!(!breaker.observe("run_shell", err));
        assert!(
            breaker.observe("run_shell", err),
            "trips on the new run of three"
        );
    }

    proptest::proptest! {
        // The incremental detector flags a loop exactly when the batch reference
        // finds a repeated failing signature, regardless of interleaving.
        #[test]
        fn tool_loop_detector_matches_the_batch_reference(
            calls in proptest::collection::vec(
                ("[a-c]{1,2}", proptest::bool::ANY),
                0..16,
            )
        ) {
            let mut detector = ToolLoopDetector::new();
            let incremental = calls
                .iter()
                .map(|(sig, failed)| detector.observe(sig, *failed))
                .any(|tripped| tripped);
            let batch = has_tool_loop(calls.iter().map(|(sig, failed)| (sig.as_str(), *failed)));
            proptest::prop_assert_eq!(incremental, batch);
        }
    }
}
