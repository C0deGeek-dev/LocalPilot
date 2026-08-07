//! Context-aware bad-output detection.

use std::collections::{HashMap, HashSet, VecDeque};

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

/// Default sliding-window size for the novelty signal: distinct successful-call
/// signatures are counted over this many recent successful calls.
pub const NO_PROGRESS_WINDOW: usize = 12;
/// Default novelty floor: when the share of distinct signatures over a full
/// window drops below this, the turn is cycling a tiny set of calls.
pub const NO_PROGRESS_DISTINCT_FLOOR: f64 = 0.34;
/// Default number of times an identical `(signature, output)` successful call
/// may recur before it counts as no forward progress.
pub const NO_PROGRESS_REPEAT_THRESHOLD: usize = 3;
/// Grace dispatches the default rail allows after the one-shot no-progress nudge:
/// the call whose observation may clear the signal before the guard stops the
/// turn. Exactly one — a small constant, not config (thresholds stay constants).
pub const NO_PROGRESS_GRACE_CALLS: usize = 1;

/// Stable 64-bit digest of one string, for compact within-turn keys.
fn digest(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

/// Stable 64-bit digest of a `(signature, output)` pair. The unit separator
/// cannot collide with the signature's own bytes, so distinct pairs stay
/// distinct even when one half is a prefix of the other.
fn digest_pair(signature: &str, output: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    signature.hash(&mut hasher);
    '\u{1f}'.hash(&mut hasher);
    output.hash(&mut hasher);
    hasher.finish()
}

/// The detector's current no-progress signal, recomputed on every successful
/// observation so a genuinely novel call can clear it — replacing the old
/// irreversible latch. `StuckRepeat` and `NoveltyDecay` carry the numbers that
/// distinguish which signal fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoProgressSignal {
    /// No no-progress signal is active for the most recent observation.
    None,
    /// The same `(signature, output)` has succeeded `count` times (at or past the
    /// repeat threshold).
    StuckRepeat { count: usize },
    /// Over the current window the share of distinct signatures fell below the
    /// floor: `distinct` distinct signatures across a window of `window` calls.
    NoveltyDecay { distinct: usize, window: usize },
}

impl NoProgressSignal {
    /// Whether any no-progress signal is currently active.
    #[must_use]
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Detects *successful* tool calls that make no forward progress — the case the
/// failure breakers ([`ToolLoopDetector`], [`RepeatedErrorBreaker`]) cannot see
/// because every call returns success. Two deterministic signals:
///
/// - **Stuck repeat:** the same `(signature, output)` succeeds `repeat_threshold`
///   times. A call whose arguments differ (a new signature) or whose output
///   differs (the world changed between calls — e.g. a re-read after an edit)
///   resets that pair's count, so a legitimate re-read is not flagged.
/// - **Novelty decay:** over the last `window` successful calls, the share of
///   distinct signatures falls below `distinct_floor` — the turn is cycling a
///   tiny set of calls even when their outputs vary.
///
/// The caller supplies an opaque signature (e.g. tool name + arguments) and the
/// call's output text, keeping this detector free of any tool or JSON
/// dependency, like [`ToolLoopDetector`].
#[derive(Debug, Clone)]
pub struct NoProgressDetector {
    window: usize,
    distinct_floor: f64,
    repeat_threshold: usize,
    /// Repeat count per `(signature, output)` digest seen this turn.
    repeats: HashMap<u64, usize>,
    /// Recent successful-call signature digests, newest at the back.
    recent: VecDeque<u64>,
    /// The no-progress signal from the most recent observation, recomputed each
    /// call so a genuinely novel call clears it. This DYNAMIC view drives the
    /// default rail's always-on guard (with the one grace dispatch); the explicit
    /// budget controller instead reads the monotone [`Self::tripped_since_reset`]
    /// view via [`Self::is_tripped`].
    active: NoProgressSignal,
    /// Monotone-since-reset compatibility view for the explicit budget controller
    /// ([`Self::is_tripped`]): set once any signal fires and NOT cleared by a
    /// novel-call recompute, so the cost controller sees exactly the old latch
    /// behaviour (a turn that trips below `soft_start` still stops at `soft_start`
    /// even if the dynamic signal has since cleared). Cleared by [`Self::reset`]
    /// (future user steering), unlike the per-turn `nudged`.
    tripped_since_reset: bool,
    /// Set once when the one-shot strategy-change nudge is emitted, monotone for
    /// the whole turn: a recovery or re-trip never emits a second nudge and never
    /// mints a second grace. Preserved across [`Self::reset`] — only a freshly
    /// constructed detector (a new turn) clears it.
    nudged: bool,
    /// Grace dispatches remaining, minted to [`NO_PROGRESS_GRACE_CALLS`] with the
    /// nudge on the default rail: the call(s) after the nudge may still dispatch so
    /// an observation can clear the signal. Consumed by the guard; never re-minted
    /// within the turn.
    grace_remaining: usize,
}

impl Default for NoProgressDetector {
    fn default() -> Self {
        Self::new(
            NO_PROGRESS_WINDOW,
            NO_PROGRESS_DISTINCT_FLOOR,
            NO_PROGRESS_REPEAT_THRESHOLD,
        )
    }
}

impl NoProgressDetector {
    /// A detector with explicit tuning. `window` is clamped to at least 1,
    /// `distinct_floor` to `0.0..=1.0`, and `repeat_threshold` to at least 1.
    #[must_use]
    pub fn new(window: usize, distinct_floor: f64, repeat_threshold: usize) -> Self {
        Self {
            window: window.max(1),
            distinct_floor: distinct_floor.clamp(0.0, 1.0),
            repeat_threshold: repeat_threshold.max(1),
            repeats: HashMap::new(),
            recent: VecDeque::new(),
            active: NoProgressSignal::None,
            tripped_since_reset: false,
            nudged: false,
            grace_remaining: 0,
        }
    }

    /// Observe one *successful* tool call. Recomputes the typed active signal
    /// (read it with [`Self::active_signal`]) from this observation, so a
    /// genuinely novel call clears it. Returns `true` only on the call that
    /// first crosses a no-progress signal this turn — the moment to surface the
    /// one-shot strategy-change hint — and on that crossing mints exactly one
    /// grace dispatch. The nudge/grace are monotone for the turn: a recovery or
    /// re-trip never re-fires or re-mints.
    pub fn observe(&mut self, signature: &str, output: &str) -> bool {
        let pair = digest_pair(signature, output);
        let count = self.repeats.entry(pair).or_insert(0);
        *count += 1;
        let stuck_count = *count;
        let stuck_repeat = stuck_count >= self.repeat_threshold;

        self.recent.push_back(digest(signature));
        while self.recent.len() > self.window {
            self.recent.pop_front();
        }
        let window_full = self.recent.len() >= self.window;
        let novelty_decayed = window_full && self.distinct_ratio() < self.distinct_floor;

        // Recompute the active signal from THIS observation — not a latch — so a
        // novel pair clears it. Stuck-repeat takes precedence when both fire.
        self.active = if stuck_repeat {
            NoProgressSignal::StuckRepeat { count: stuck_count }
        } else if novelty_decayed {
            NoProgressSignal::NoveltyDecay {
                distinct: self.distinct_count(),
                window: self.recent.len(),
            }
        } else {
            NoProgressSignal::None
        };

        // Maintain the monotone since-reset view the explicit budget controller
        // reads: once any signal fires it stays set until reset, so the cost
        // controller keeps the old latch behaviour even after the dynamic signal
        // clears.
        self.tripped_since_reset |= self.active.is_active();

        // Fire the one-shot nudge on the first crossing this turn and mint the
        // grace with it. `nudged` is monotone and the grace is minted once, so a
        // recovery or re-trip never re-fires or re-mints.
        let crossing = self.active.is_active() && !self.nudged;
        if crossing {
            self.nudged = true;
            self.grace_remaining = NO_PROGRESS_GRACE_CALLS;
        }
        crossing
    }

    /// Distinct signatures over the current window.
    fn distinct_count(&self) -> usize {
        self.recent.iter().collect::<HashSet<_>>().len()
    }

    /// Share of distinct signatures over the current window (`1.0` when empty).
    fn distinct_ratio(&self) -> f64 {
        if self.recent.is_empty() {
            return 1.0;
        }
        self.distinct_count() as f64 / self.recent.len() as f64
    }

    /// The current typed no-progress signal, recomputed on each observation.
    #[must_use]
    pub fn active_signal(&self) -> NoProgressSignal {
        self.active
    }

    /// Compatibility accessor for the explicit budget controller: the MONOTONE
    /// since-reset view — `true` once any no-progress signal has fired this turn,
    /// and NOT cleared by a novel-call recompute. This preserves the old latch
    /// behaviour exactly (a turn that trips below `soft_start` still stops at
    /// `soft_start`). The default rail's always-on guard instead reads the dynamic
    /// [`Self::active_signal`], which can clear on a novel call. Cleared by
    /// [`Self::reset`].
    #[must_use]
    pub fn is_tripped(&self) -> bool {
        self.tripped_since_reset
    }

    /// Consume the one pending grace dispatch, if any, returning whether a grace
    /// was available (and is now spent). The default-rail guard calls this when a
    /// signal is active: `true` lets exactly one more call dispatch so its
    /// observation can clear the signal; `false` means the grace is spent and the
    /// turn must stop.
    pub fn consume_grace(&mut self) -> bool {
        if self.grace_remaining > 0 {
            self.grace_remaining -= 1;
            true
        } else {
            false
        }
    }

    /// Whether the one-shot nudge has already been emitted this turn (monotone).
    #[must_use]
    pub fn has_nudged(&self) -> bool {
        self.nudged
    }

    /// Clear the observation history and active signal, and cancel any pending
    /// grace — e.g. when the user steers the turn — while PRESERVING the spent
    /// per-turn `nudged` state, so a steer never re-mints a nudge or a grace. A
    /// freshly constructed detector (a new turn) is the only full reset of the
    /// nudge/grace lifecycle.
    pub fn reset(&mut self) {
        self.repeats.clear();
        self.recent.clear();
        self.active = NoProgressSignal::None;
        self.tripped_since_reset = false;
        self.grace_remaining = 0;
        // `nudged` is intentionally NOT cleared (monotone for the turn).
    }
}

/// The budget controller's decision for whether the next tool call may run this
/// turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetDecision {
    /// The next call may run.
    Continue,
    /// Stop: the turn is making no forward progress and has reached the soft
    /// start — the point a stuck turn is no longer worth extending.
    StopNoProgress,
    /// Stop: the hard cost-contract ceiling is reached, regardless of progress.
    StopCostMax,
}

/// Turns the per-turn tool-call ceiling into a progress-aware bound. A turn that
/// keeps making progress runs up to `hard_max`; a turn flagged as making no
/// progress stops at `soft_start`. `hard_max` is always honoured, so a turn can
/// never loop unbounded even when the progress signal is (wrongly) positive —
/// the cost contract holds independent of any heuristic's confidence.
///
/// With `hard_max == soft_start` the bound stops at exactly the soft start
/// regardless of progress: the flat fixed-ceiling behaviour. Raising `hard_max`
/// above the soft start opts a deployment into the adaptive extension.
///
/// [`BudgetController::Unlimited`] disables the bound entirely — every call may
/// run. This is the default when no `tool_call_budget` is configured, so the
/// budget is opt-in rather than a built-in cap.
#[derive(Debug, Clone, Copy)]
pub enum BudgetController {
    /// Budget disabled: every call may run, with no cost ceiling and no
    /// no-progress stop. The turn is bounded only by its other exit paths.
    Unlimited,
    /// Budget enabled with a progress-aware soft start and a hard cost ceiling.
    Bounded { soft_start: usize, hard_max: usize },
}

impl BudgetController {
    /// Build a controller from the configured budget. With both bounds unset the
    /// budget is disabled ([`BudgetController::Unlimited`]). When only one is set
    /// it serves as both the soft start and the hard ceiling. `hard_max` is
    /// clamped up to `soft_start`, so a misconfigured `max < start` never stops
    /// every turn below its soft start.
    #[must_use]
    pub fn new(soft_start: Option<usize>, hard_max: Option<usize>) -> Self {
        match (soft_start, hard_max) {
            (None, None) => Self::Unlimited,
            (Some(soft), Some(hard)) => Self::Bounded {
                soft_start: soft,
                hard_max: hard.max(soft),
            },
            (Some(soft), None) => Self::Bounded {
                soft_start: soft,
                hard_max: soft,
            },
            (None, Some(hard)) => Self::Bounded {
                soft_start: hard,
                hard_max: hard,
            },
        }
    }

    /// Decide whether the next call may run, given how many calls this turn have
    /// already executed (`calls_used`) and whether the turn is making no forward
    /// progress. The cost-max ceiling is checked first, so it always wins. An
    /// [`BudgetController::Unlimited`] budget always continues.
    #[must_use]
    pub fn decide(&self, calls_used: usize, no_progress: bool) -> BudgetDecision {
        match self {
            Self::Unlimited => BudgetDecision::Continue,
            Self::Bounded {
                soft_start,
                hard_max,
            } => {
                if calls_used >= *hard_max {
                    BudgetDecision::StopCostMax
                } else if no_progress && calls_used >= *soft_start {
                    BudgetDecision::StopNoProgress
                } else {
                    BudgetDecision::Continue
                }
            }
        }
    }

    /// The hard cost-contract ceiling (after clamping), or `None` when the budget
    /// is disabled.
    #[must_use]
    pub fn hard_max(&self) -> Option<usize> {
        match self {
            Self::Unlimited => None,
            Self::Bounded { hard_max, .. } => Some(*hard_max),
        }
    }

    /// The soft start: the count past which a no-progress turn stops, or `None`
    /// when the budget is disabled.
    #[must_use]
    pub fn soft_start(&self) -> Option<usize> {
        match self {
            Self::Unlimited => None,
            Self::Bounded { soft_start, .. } => Some(*soft_start),
        }
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

    #[test]
    fn no_progress_trips_on_repeated_successful_signature() {
        let mut detector = NoProgressDetector::new(12, 0.34, 3);
        let sig = "read_file\u{1f}{\"path\":\"a.rs\"}";
        let out = "the same file contents";
        assert!(!detector.observe(sig, out), "1st identical success");
        assert!(!detector.observe(sig, out), "2nd identical success");
        assert!(
            detector.observe(sig, out),
            "3rd identical success crosses the no-progress threshold"
        );
        assert!(detector.is_tripped());
        // Fires once on the crossing, not on every later repeat.
        assert!(!detector.observe(sig, out), "no re-fire while still stuck");
        assert!(
            detector.is_tripped(),
            "still active while stuck (no novel call has cleared it)"
        );
    }

    #[test]
    fn state_change_between_repeats_resets_no_progress() {
        // A re-read after an edit returns *different* output: the read signature
        // repeats, but the (signature, output) pair is new each time, so it is
        // progress, not a loop.
        let mut detector = NoProgressDetector::new(12, 0.34, 3);
        let sig = "read_file\u{1f}{\"path\":\"a.rs\"}";
        assert!(!detector.observe(sig, "version 1"));
        assert!(!detector.observe(sig, "version 2"));
        assert!(!detector.observe(sig, "version 3"));
        assert!(
            !detector.is_tripped(),
            "changing output between repeats is forward progress"
        );
    }

    #[test]
    fn varied_signatures_do_not_trip_no_progress() {
        let mut detector = NoProgressDetector::new(12, 0.34, 3);
        for i in 0..20 {
            let sig = format!("read_file\u{1f}{{\"path\":\"f{i}.rs\"}}");
            assert!(!detector.observe(&sig, "contents"));
        }
        assert!(
            !detector.is_tripped(),
            "a high-novelty sequence of distinct calls is not a loop"
        );
    }

    #[test]
    fn novelty_decay_trips_on_a_small_cycle_with_varying_output() {
        // Two signatures alternating, each with a unique output so the stuck-
        // repeat signal never fires — only the novelty signal can trip here.
        let mut detector = NoProgressDetector::new(12, 0.34, 100);
        let mut tripped = false;
        for i in 0..12 {
            let sig = if i % 2 == 0 {
                "tool_a\u{1f}{}"
            } else {
                "tool_b\u{1f}{}"
            };
            let out = format!("unique output {i}");
            tripped |= detector.observe(sig, &out);
        }
        assert!(
            tripped && detector.is_tripped(),
            "cycling two calls over a full window decays novelty below the floor"
        );
    }

    #[test]
    fn no_progress_reset_clears_state() {
        let mut detector = NoProgressDetector::new(12, 0.34, 2);
        let sig = "read_file\u{1f}{\"path\":\"a.rs\"}";
        assert!(!detector.observe(sig, "x"));
        assert!(detector.observe(sig, "x"), "trips on the repeat");
        detector.reset();
        assert!(
            !detector.is_tripped(),
            "reset clears the active signal and observation history"
        );
        assert!(
            !detector.observe(sig, "x"),
            "streak restarts after a turn boundary"
        );
    }

    #[test]
    fn active_signal_reports_typed_stuck_repeat() {
        let mut d = NoProgressDetector::new(12, 0.34, 3);
        let sig = "read_file\u{1f}{\"path\":\"a\"}";
        d.observe(sig, "same");
        d.observe(sig, "same");
        d.observe(sig, "same");
        assert_eq!(
            d.active_signal(),
            NoProgressSignal::StuckRepeat { count: 3 }
        );
    }

    #[test]
    fn active_signal_reports_typed_novelty_decay() {
        // repeat_threshold high so only novelty can fire; two signatures over a
        // full window of 12 → 2 distinct across 12 calls.
        let mut d = NoProgressDetector::new(12, 0.34, 100);
        for i in 0..12 {
            let sig = if i % 2 == 0 { "a\u{1f}{}" } else { "b\u{1f}{}" };
            d.observe(sig, &format!("out {i}"));
        }
        assert_eq!(
            d.active_signal(),
            NoProgressSignal::NoveltyDecay {
                distinct: 2,
                window: 12
            }
        );
    }

    #[test]
    fn novel_success_clears_the_dynamic_signal_but_not_the_monotone_view() {
        let mut d = NoProgressDetector::new(12, 0.34, 3);
        let sig = "read_file\u{1f}{\"path\":\"a\"}";
        d.observe(sig, "same");
        d.observe(sig, "same");
        assert!(d.observe(sig, "same"), "crosses on the 3rd repeat");
        assert!(d.active_signal().is_active());
        assert!(d.is_tripped(), "the since-reset view sets on the crossing");
        // A genuinely novel pair recomputes the DYNAMIC signal to None (the
        // default rail can recover), but the MONOTONE since-reset view that the
        // explicit budget controller reads stays set until a reset.
        let novel = "write_file\u{1f}{\"path\":\"b\"}";
        assert!(
            !d.observe(novel, "brand new"),
            "novel call does not re-nudge"
        );
        assert_eq!(
            d.active_signal(),
            NoProgressSignal::None,
            "the dynamic signal cleared on the novel call"
        );
        assert!(
            d.is_tripped(),
            "but the monotone budget-controller view remains set"
        );
        // A reset (future user steering) clears the monotone view too.
        d.reset();
        assert!(!d.is_tripped(), "reset clears the since-reset view");
    }

    #[test]
    fn nudge_and_grace_fire_exactly_once_per_turn() {
        let mut d = NoProgressDetector::new(12, 0.34, 3);
        let sig = "read_file\u{1f}{\"path\":\"a\"}";
        d.observe(sig, "same");
        d.observe(sig, "same");
        assert!(d.observe(sig, "same"), "one-shot nudge on the crossing");
        assert!(d.has_nudged());
        // Grace mints exactly once and is consumed once.
        assert!(d.consume_grace(), "one grace dispatch was minted");
        assert!(!d.consume_grace(), "no second grace");
        // Staying stuck never re-fires the nudge or re-mints grace.
        assert!(!d.observe(sig, "same"), "no second nudge while stuck");
        assert!(!d.consume_grace(), "still no new grace after a re-trip");
        assert!(d.has_nudged(), "nudged stays monotone");
    }

    #[test]
    fn recovered_then_retripped_mints_no_second_grace() {
        let mut d = NoProgressDetector::new(12, 0.34, 3);
        let a = "read_file\u{1f}{\"path\":\"a\"}";
        d.observe(a, "same");
        d.observe(a, "same");
        assert!(d.observe(a, "same"), "trips + nudges + mints grace");
        assert!(d.consume_grace(), "grace spent on the recovery dispatch");
        // Recover on a novel pair.
        assert!(!d.observe("b\u{1f}{}", "new"));
        assert_eq!(d.active_signal(), NoProgressSignal::None);
        // Re-trip later: the signal returns but there is no new nudge / grace.
        d.observe(a, "same");
        assert!(!d.observe(a, "same"), "re-trip does not re-nudge");
        assert!(d.active_signal().is_active(), "signal is active again");
        assert!(
            !d.consume_grace(),
            "no second grace after recovery + re-trip"
        );
    }

    #[test]
    fn reset_preserves_nudged_and_cancels_pending_grace() {
        let mut d = NoProgressDetector::new(12, 0.34, 2);
        let sig = "read_file\u{1f}{\"path\":\"a\"}";
        d.observe(sig, "x");
        assert!(d.observe(sig, "x"), "trips + mints grace");
        assert!(d.has_nudged());
        assert!(d.is_tripped(), "the since-reset view is set before reset");
        d.reset();
        // History + active signal + the monotone view + pending grace are cleared…
        assert_eq!(d.active_signal(), NoProgressSignal::None);
        assert!(!d.is_tripped(), "reset clears the since-reset view");
        assert!(!d.consume_grace(), "reset cancelled the pending grace");
        // …but the spent nudge is preserved, so a re-trip after the steer does
        // not mint a fresh nudge or grace.
        assert!(d.has_nudged(), "reset preserves the spent nudge");
        d.observe(sig, "x");
        assert!(
            !d.observe(sig, "x"),
            "re-trip after reset does not re-nudge"
        );
        assert!(!d.consume_grace(), "no fresh grace after reset + re-trip");
    }

    #[test]
    fn no_progress_observe_is_cheap() {
        // The detector hashes the signature and output once per call — no
        // filesystem walk — so its per-call cost is negligible. Time a large
        // batch and assert a generous ceiling to catch a pathological regression.
        let mut detector = NoProgressDetector::default();
        let iterations = 100_000u32;
        let start = std::time::Instant::now();
        for i in 0..iterations {
            let signature = format!("read_file\u{1f}{{\"path\":\"f{}.rs\"}}", i % 64);
            detector.observe(&signature, "some representative tool output line");
        }
        let elapsed = start.elapsed();
        eprintln!("no-progress observe: {iterations} calls in {elapsed:?}");
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "100k observes took {elapsed:?}; expected well under 2s"
        );
    }

    #[test]
    fn budget_continues_below_the_soft_start_even_when_stuck() {
        let controller = BudgetController::new(Some(50), Some(200));
        assert_eq!(controller.decide(0, false), BudgetDecision::Continue);
        assert_eq!(
            controller.decide(49, true),
            BudgetDecision::Continue,
            "a stuck turn still runs up to the soft start"
        );
    }

    #[test]
    fn budget_stops_a_stuck_turn_at_the_soft_start() {
        let controller = BudgetController::new(Some(50), Some(200));
        assert_eq!(controller.decide(50, true), BudgetDecision::StopNoProgress);
    }

    #[test]
    fn budget_lets_a_productive_turn_run_past_the_soft_start_to_the_max() {
        let controller = BudgetController::new(Some(50), Some(200));
        assert_eq!(controller.decide(50, false), BudgetDecision::Continue);
        assert_eq!(controller.decide(199, false), BudgetDecision::Continue);
        assert_eq!(controller.decide(200, false), BudgetDecision::StopCostMax);
    }

    #[test]
    fn cost_max_always_stops_regardless_of_progress() {
        let controller = BudgetController::new(Some(50), Some(200));
        assert_eq!(controller.decide(200, false), BudgetDecision::StopCostMax);
        assert_eq!(
            controller.decide(200, true),
            BudgetDecision::StopCostMax,
            "the hard cost ceiling stops even a turn the signal still calls stuck"
        );
        assert_eq!(
            controller.decide(10_000, false),
            BudgetDecision::StopCostMax
        );
    }

    #[test]
    fn hard_max_is_clamped_up_to_the_soft_start() {
        let controller = BudgetController::new(Some(50), Some(10));
        assert_eq!(
            controller.hard_max(),
            Some(50),
            "max below start is clamped up"
        );
        assert_eq!(controller.decide(50, false), BudgetDecision::StopCostMax);
    }

    #[test]
    fn equal_bounds_reproduce_the_flat_fixed_ceiling() {
        let controller = BudgetController::new(Some(50), Some(50));
        assert_eq!(controller.decide(49, true), BudgetDecision::Continue);
        assert_eq!(controller.decide(50, false), BudgetDecision::StopCostMax);
        assert_eq!(
            controller.decide(50, true),
            BudgetDecision::StopCostMax,
            "with max == start the no-progress path never pre-empts the ceiling"
        );
    }

    #[test]
    fn a_single_configured_bound_serves_as_both_start_and_ceiling() {
        let soft_only = BudgetController::new(Some(7), None);
        assert_eq!(
            soft_only.hard_max(),
            Some(7),
            "soft start doubles as ceiling"
        );
        assert_eq!(soft_only.decide(7, false), BudgetDecision::StopCostMax);

        let max_only = BudgetController::new(None, Some(7));
        assert_eq!(
            max_only.soft_start(),
            Some(7),
            "ceiling doubles as soft start"
        );
        assert_eq!(max_only.decide(7, false), BudgetDecision::StopCostMax);
    }

    #[test]
    fn an_unset_budget_is_unlimited_and_always_continues() {
        let controller = BudgetController::new(None, None);
        assert!(matches!(controller, BudgetController::Unlimited));
        assert_eq!(controller.hard_max(), None);
        assert_eq!(controller.soft_start(), None);
        assert_eq!(
            controller.decide(1_000_000, true),
            BudgetDecision::Continue,
            "a disabled budget never stops a turn, even a stuck one"
        );
    }
}
