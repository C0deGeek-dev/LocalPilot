//! The machine-readable capability scorecard: the cross-corpus contract a
//! headless run emits so a benchmark can grade the *harness* on three layers —
//! results, code quality, and process — rather than a single pass/fail bit.
//!
//! The shape here is the contract both corpora honour: the in-repo first-party
//! runner and the external (LocalBench-driven) runner each produce one
//! [`Scorecard`] per task run and serialize it as JSON. The one-line
//! [`crate::DisciplineMetrics::scorecard_line`] text scorecard stays for
//! back-compat with the existing live-model pipeline; this is the structured
//! superset.
//!
//! Two of the three layers are derived here, deterministically, from artefacts
//! the loop already produces:
//! - [`extract_process`] reads the session event log + the [`EvidenceLedger`]
//!   projection to compute the `process` block (no model in the loop).
//! - [`QualityBlock::from_signals`] assembles the `quality` block from a captured
//!   diff, the gate's [`CheckOutcome`]s, and the diff-derived helpers below.
//!
//! The `results` and `speed` blocks are graded/measured by the runner (the test
//! verdict and wall-clock are the runner's to supply), so they are plain data
//! the caller fills in.

use serde::{Deserialize, Serialize};

use localpilot_store::{SessionEvent, SessionEventKind};

use crate::discipline::DisciplineMetrics;
use crate::evidence::{CallOutcome, EvidenceLedger};
use crate::quality::CheckOutcome;

/// The scorecard contract version. Bump on any breaking shape change (a removed
/// or renamed field); additive fields keep the version.
pub const SCORECARD_SCHEMA: u32 = 1;

/// One task run, graded on three layers plus a reported speed guardrail.
///
/// `speed` is a guardrail, never the headline metric — correctness gates, then
/// quality and process rank (the composite lives in the ablation subject).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scorecard {
    /// Contract version ([`SCORECARD_SCHEMA`]).
    pub schema: u32,
    /// Task identifier (corpus-local, e.g. a first-party task name or an
    /// external instance id).
    pub task: String,
    /// The harness arm this run used (e.g. `full`, `baseline`, `no-retrieval`),
    /// so an ablation can group runs by configuration.
    pub arm: String,
    /// The model id the run used, or `fake` for the offline deterministic path.
    pub model: String,
    /// Did the change resolve the task, and is it regression-safe?
    pub results: ResultsBlock,
    /// Static code-quality signals on the produced diff.
    pub quality: QualityBlock,
    /// How the agent worked: tool economy, discipline, retrieval, recovery.
    pub process: ProcessBlock,
    /// Reported speed/cost guardrail. Never the headline score.
    pub speed: SpeedBlock,
}

impl Scorecard {
    /// Serialize the scorecard to its canonical JSON string (the wire contract).
    ///
    /// # Errors
    /// Returns the `serde_json` error if serialization fails (it does not for
    /// this all-owned, finite type, but the contract is fallible by signature).
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

/// The results layer: did the work get done, safely?
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultsBlock {
    /// The task's own test(s) passed after the change.
    pub passed: bool,
    /// No previously-passing test regressed (the `PASS_TO_PASS`/regression set
    /// still passes). Vacuously `true` when a corpus carries no regression set.
    pub regression_safe: bool,
    /// Fractional credit in `0.0..=1.0` for a partially-solved task (e.g. the
    /// fraction of target tests flipped). `1.0` on a full pass, `0.0` on no
    /// progress.
    pub partial_credit: f64,
    /// Target tests the task graded against.
    pub tests_total: u32,
    /// Of those, how many passed after the change.
    pub tests_passed: u32,
}

/// The code-quality layer: static signals on the produced diff.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualityBlock {
    /// Added lines in the produced diff.
    pub diff_added: u32,
    /// Removed lines in the produced diff.
    pub diff_removed: u32,
    /// Files the diff touches.
    pub diff_files: u32,
    /// Candidate churn relative to the gold patch (`(added+removed) /
    /// gold(added+removed)`); `null` when there is no gold patch or the gold
    /// patch is empty. A ratio near `1.0` is minimal; large is bloated.
    pub vs_gold_ratio: Option<f64>,
    /// `cargo fmt --check` (or the stack's formatter check) passed.
    pub format_clean: bool,
    /// The linter (clippy / equivalent) reported no findings.
    pub lint_clean: bool,
    /// The type/compile check passed.
    pub typecheck_clean: bool,
    /// Added cyclomatic-ish complexity, a best-effort diff-derived proxy;
    /// `null` when not computed.
    pub complexity_delta: Option<i64>,
    /// The diff added at least one test.
    pub tests_added: bool,
}

impl QualityBlock {
    /// Assemble the quality block from a captured diff, an optional gold diff,
    /// the gate's check outcomes, and the diff-derived complexity/tests signals.
    ///
    /// The `format_clean` / `lint_clean` / `typecheck_clean` flags are read from
    /// `checks` by conventional name (`fmt`/`format`, `clippy`/`lint`,
    /// `check`/`typecheck`/`test`-adjacent); an absent check is treated as clean
    /// (it did not report a finding), so a corpus that runs only a subset of the
    /// gate still produces a well-formed block.
    #[must_use]
    pub fn from_signals(
        diff: &DiffStat,
        gold: Option<&DiffStat>,
        checks: &[CheckOutcome],
        complexity_delta: Option<i64>,
        tests_added: bool,
    ) -> Self {
        let candidate_churn = diff.added + diff.removed;
        let vs_gold_ratio = gold.and_then(|g| {
            let gold_churn = g.added + g.removed;
            if gold_churn == 0 {
                None
            } else {
                Some(f64::from(candidate_churn) / f64::from(gold_churn))
            }
        });
        Self {
            diff_added: diff.added,
            diff_removed: diff.removed,
            diff_files: diff.files,
            vs_gold_ratio,
            format_clean: check_clean(checks, &["fmt", "format"]),
            lint_clean: check_clean(checks, &["clippy", "lint"]),
            typecheck_clean: check_clean(checks, &["check", "typecheck", "build"]),
            complexity_delta,
            tests_added,
        }
    }
}

/// The process layer: how the agent worked, derived from the trace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessBlock {
    /// Total tool calls the run made.
    pub tool_calls: u32,
    /// Calls that repeated an earlier identical `(tool, arguments)` call.
    pub redundant_calls: u32,
    /// An observation call (read/search/run/status) preceded the first mutating
    /// call — the agent looked before it edited. Vacuously `true` with no edit.
    pub reproduce_before_fix: bool,
    /// A test-like call appears in the trace before the final claim.
    pub test_before_done: bool,
    /// Retrieval contributed to the run (memories surfaced, or a retrieval tool
    /// was called).
    pub retrieval_used: bool,
    /// How many memories/knowledge chunks were surfaced and used across the run.
    pub retrieval_count: u32,
    /// The recorded turn stop label (`StopReason` serialized, e.g. `Done`,
    /// `BudgetExceeded`, `NoProgress`), or `unknown` when none was recorded.
    pub exit_reason: String,
    /// After a failed call, a later grounded success followed — the agent
    /// recovered rather than giving up or claiming on the failure.
    pub recovered_after_failure: bool,
    /// The per-capability discipline rates, when a cross-scenario rollup is
    /// attached (the single-run extractor leaves this `null`).
    pub discipline: Option<DisciplineMetrics>,
}

/// The reported speed/cost guardrail. Never the headline metric (17h).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeedBlock {
    /// Wall-clock duration of the run, in milliseconds (runner-measured).
    pub wall_ms: u64,
    /// Input tokens reported across the run.
    pub input_tokens: u64,
    /// Output tokens reported across the run.
    pub output_tokens: u64,
}

impl SpeedBlock {
    /// Sum the token usage reported in the event log; `wall_ms` is the runner's
    /// own measurement and is supplied separately.
    #[must_use]
    pub fn from_events(events: &[SessionEvent], wall_ms: u64) -> Self {
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        for event in events {
            if let SessionEventKind::UsageReported {
                input_tokens: input,
                output_tokens: output,
            } = &event.kind
            {
                input_tokens += input;
                output_tokens += output;
            }
        }
        Self {
            wall_ms,
            input_tokens,
            output_tokens,
        }
    }
}

/// Compute the deterministic `process` block from a session event sequence and
/// its [`EvidenceLedger`] projection. No model runs; the same inputs always
/// yield the same block, so it is safe for offline CI.
#[must_use]
pub fn extract_process(events: &[SessionEvent], ledger: &EvidenceLedger) -> ProcessBlock {
    let calls = ledger.calls();
    let tool_calls = calls.len() as u32;

    // Redundant: a call repeating an earlier identical (name + input) call.
    let mut redundant_calls = 0u32;
    for (i, call) in calls.iter().enumerate() {
        if calls[..i]
            .iter()
            .any(|earlier| earlier.name == call.name && earlier.input == call.input)
        {
            redundant_calls += 1;
        }
    }

    // Reproduce-before-fix: an observation call precedes the first mutation.
    let reproduce_before_fix = match calls.iter().position(|c| is_mutation(&c.name)) {
        Some(idx) => calls[..idx].iter().any(|c| !is_mutation(&c.name)),
        None => true,
    };

    // Test-before-done: a test-like call appears anywhere in the trace.
    let test_before_done = calls.iter().any(|c| is_test_like(&c.name, &c.input));

    // Retrieval utilization: memories surfaced, plus retrieval-type tool calls.
    let mut retrieval_count = 0u32;
    for event in events {
        if let SessionEventKind::MemoriesUsed { memories } = &event.kind {
            retrieval_count += memories.len() as u32;
        }
    }
    let retrieval_used = retrieval_count > 0 || calls.iter().any(|c| is_retrieval(&c.name));

    // Recovered-after-failure: an error, then a later grounded success.
    let recovered_after_failure = match calls.iter().position(|c| c.outcome == CallOutcome::Error) {
        Some(idx) => calls[idx + 1..]
            .iter()
            .any(|c| c.outcome == CallOutcome::Ok && c.claim_referenced),
        None => false,
    };

    // Exit reason: the last recorded turn stop label.
    let exit_reason = events
        .iter()
        .rev()
        .find_map(|e| match &e.kind {
            SessionEventKind::TurnEnded { stop } => Some(stop.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "unknown".to_string());

    ProcessBlock {
        tool_calls,
        redundant_calls,
        reproduce_before_fix,
        test_before_done,
        retrieval_used,
        retrieval_count,
        exit_reason,
        recovered_after_failure,
        discipline: None,
    }
}

/// Line/file counts of a unified diff, the diff-size + blast-radius signal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffStat {
    /// Added content lines (`+`, excluding the `+++` file header).
    pub added: u32,
    /// Removed content lines (`-`, excluding the `---` file header).
    pub removed: u32,
    /// Distinct files the diff touches.
    pub files: u32,
}

impl DiffStat {
    /// Parse the line/file counts out of a unified diff (`git diff` output).
    #[must_use]
    pub fn from_unified(diff: &str) -> Self {
        let mut added = 0u32;
        let mut removed = 0u32;
        let mut files = 0u32;
        for line in diff.lines() {
            if line.starts_with("diff --git ") {
                files += 1;
            } else if line.starts_with("+++") || line.starts_with("---") {
                // File headers, not content.
            } else if line.starts_with('+') {
                added += 1;
            } else if line.starts_with('-') {
                removed += 1;
            }
        }
        Self {
            added,
            removed,
            files,
        }
    }
}

/// Whether the diff adds a test: an added line declares a test, or touches a
/// conventional test path. A best-effort, language-agnostic proxy. Markers that
/// are short common substrings (`it(`, `describe(`, `test(`) are matched only at
/// the start of a trimmed added line, so a token like `digit()` does not falsely
/// register as a JavaScript test.
#[must_use]
pub fn tests_added_in_diff(diff: &str) -> bool {
    diff.lines().any(|line| {
        let Some(added) = line.strip_prefix('+') else {
            return false;
        };
        if added.starts_with("++") {
            return false; // the `+++` file header
        }
        let lower = added.to_lowercase();
        let trimmed = lower.trim_start();
        lower.contains("#[test]")
            || lower.contains("@test")
            || trimmed.starts_with("def test_")
            || trimmed.starts_with("describe(")
            || trimmed.starts_with("it(")
            || trimmed.starts_with("test(")
            || (lower.contains("fn ") && lower.contains("test"))
    }) || diff.lines().any(|line| {
        line.starts_with("diff --git")
            && (line.contains("/tests/") || line.contains("test_") || line.contains(".test."))
    })
}

/// A best-effort added-complexity proxy: net branch/decision keywords introduced
/// by the diff (added minus removed). Not a real cyclomatic count, but a
/// deterministic, language-agnostic signal that tracks branchiness.
#[must_use]
pub fn complexity_delta_in_diff(diff: &str) -> i64 {
    const KEYWORDS: &[&str] = &[
        " if ", " for ", " while ", " match ", " case ", "&&", "||", "?", " elif ", " when ",
        " catch", " switch",
    ];
    let mut delta: i64 = 0;
    for line in diff.lines() {
        let (sign, body) = if let Some(body) = line.strip_prefix('+') {
            if body.starts_with('+') {
                continue;
            }
            (1i64, body)
        } else if let Some(body) = line.strip_prefix('-') {
            if body.starts_with('-') {
                continue;
            }
            (-1i64, body)
        } else {
            continue;
        };
        let padded = format!(" {body} ");
        let hits: i64 = KEYWORDS
            .iter()
            .map(|kw| padded.matches(kw).count() as i64)
            .sum();
        delta += sign * hits;
    }
    delta
}

/// A mutating tool — one that changes workspace files.
fn is_mutation(tool: &str) -> bool {
    matches!(
        tool,
        "write_file" | "edit_file" | "apply_patch" | "create_file"
    )
}

/// A retrieval-type tool — one that surfaces context/memory/knowledge.
fn is_retrieval(tool: &str) -> bool {
    matches!(
        tool,
        "search_text" | "knowledge_search" | "memory_search" | "search" | "grep"
    )
}

/// Whether a call looks like running a test suite: a test-runner tool, or a
/// shell/command call whose arguments invoke a test runner.
fn is_test_like(tool: &str, input: &serde_json::Value) -> bool {
    if tool.contains("test") {
        return true;
    }
    if matches!(tool, "run_shell" | "shell" | "run_command" | "bash") {
        let rendered = input.to_string().to_lowercase();
        return [
            "test",
            "pytest",
            "cargo test",
            "go test",
            "npm test",
            "jest",
            "ctest",
        ]
        .iter()
        .any(|needle| rendered.contains(needle));
    }
    false
}

/// Whether a gate check of one of the given conventional names reported a clean
/// pass. An absent check is treated as clean (it raised no finding).
fn check_clean(checks: &[CheckOutcome], names: &[&str]) -> bool {
    checks
        .iter()
        .filter(|c| names.iter().any(|n| c.name.contains(n)))
        .all(CheckOutcome::passed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_core::{ContentBlock, EventId, Message, Role, ToolCall, ToolResult};
    use localpilot_store::{MemoryUsed, MessageOrigin, SESSION_EVENT_FORMAT_VERSION};

    fn event(kind: SessionEventKind) -> SessionEvent {
        SessionEvent {
            v: SESSION_EVENT_FORMAT_VERSION,
            id: EventId::new(),
            parent_id: None,
            at_unix: 0,
            kind,
        }
    }

    fn assistant(content: Vec<ContentBlock>) -> SessionEvent {
        event(SessionEventKind::Message {
            message: Message::new(Role::Assistant, content),
            origin: MessageOrigin::Assistant,
        })
    }

    fn call(id: &str, name: &str, input: serde_json::Value) -> ContentBlock {
        ContentBlock::ToolUse(ToolCall::new(id.into(), name, input))
    }

    fn tool_result(id: &str, output: &str, is_error: bool) -> SessionEvent {
        let result = if is_error {
            ToolResult::error(id.into(), output)
        } else {
            ToolResult::success(id.into(), output)
        };
        event(SessionEventKind::Message {
            message: Message::new(Role::Tool, vec![ContentBlock::ToolResult(result)]),
            origin: MessageOrigin::ToolResult,
        })
    }

    fn sample_scorecard() -> Scorecard {
        Scorecard {
            schema: SCORECARD_SCHEMA,
            task: "fix-off-by-one".to_string(),
            arm: "full".to_string(),
            model: "fake".to_string(),
            results: ResultsBlock {
                passed: true,
                regression_safe: true,
                partial_credit: 1.0,
                tests_total: 3,
                tests_passed: 3,
            },
            quality: QualityBlock {
                diff_added: 4,
                diff_removed: 2,
                diff_files: 1,
                vs_gold_ratio: Some(1.5),
                format_clean: true,
                lint_clean: true,
                typecheck_clean: true,
                complexity_delta: Some(0),
                tests_added: false,
            },
            process: ProcessBlock {
                tool_calls: 3,
                redundant_calls: 0,
                reproduce_before_fix: true,
                test_before_done: true,
                retrieval_used: true,
                retrieval_count: 2,
                exit_reason: "Done".to_string(),
                recovered_after_failure: false,
                discipline: None,
            },
            speed: SpeedBlock {
                wall_ms: 1200,
                input_tokens: 500,
                output_tokens: 200,
            },
        }
    }

    #[test]
    fn scorecard_contract_round_trips_through_json() {
        let card = sample_scorecard();
        let json = card.to_json().expect("serialize");
        let back: Scorecard = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(card, back);
    }

    #[test]
    fn scorecard_json_carries_all_three_layers_and_speed() {
        let json = sample_scorecard().to_json().expect("serialize");
        let value: serde_json::Value = serde_json::from_str(&json).expect("parse");
        for key in [
            "schema", "task", "arm", "model", "results", "quality", "process", "speed",
        ] {
            assert!(value.get(key).is_some(), "scorecard must carry `{key}`");
        }
        // Nullable contract fields serialize as present (null), not omitted.
        let mut minimal = sample_scorecard();
        minimal.quality.vs_gold_ratio = None;
        minimal.quality.complexity_delta = None;
        minimal.process.discipline = None;
        let value: serde_json::Value =
            serde_json::from_str(&minimal.to_json().expect("serialize")).expect("parse");
        assert!(value["quality"]["vs_gold_ratio"].is_null());
        assert!(value["quality"]["complexity_delta"].is_null());
        assert!(value["process"]["discipline"].is_null());
    }

    #[test]
    fn process_extractor_counts_calls_and_redundancy() {
        let events = vec![
            assistant(vec![call(
                "c1",
                "read_file",
                serde_json::json!({ "path": "a.rs" }),
            )]),
            tool_result("c1", "fn a() {}", false),
            assistant(vec![call(
                "c2",
                "read_file",
                serde_json::json!({ "path": "a.rs" }),
            )]),
            tool_result("c2", "fn a() {}", false),
            assistant(vec![call(
                "c3",
                "write_file",
                serde_json::json!({ "path": "a.rs", "content": "fn a() { 1 }" }),
            )]),
            tool_result("c3", "ok", false),
            event(SessionEventKind::TurnEnded {
                stop: "Done".to_string(),
            }),
        ];
        let ledger = EvidenceLedger::project(&events);
        let process = extract_process(&events, &ledger);
        assert_eq!(process.tool_calls, 3);
        assert_eq!(
            process.redundant_calls, 1,
            "the second identical read repeats"
        );
        assert!(process.reproduce_before_fix, "a read preceded the write");
        assert_eq!(process.exit_reason, "Done");
    }

    #[test]
    fn process_extractor_sees_retrieval_and_recovery() {
        let events = vec![
            event(SessionEventKind::MemoriesUsed {
                memories: vec![
                    MemoryUsed {
                        id: "m1".into(),
                        score: 5,
                        layer: "memory".into(),
                    },
                    MemoryUsed {
                        id: "m2".into(),
                        score: 3,
                        layer: "index".into(),
                    },
                ],
            }),
            assistant(vec![call("e1", "read_file", serde_json::json!({}))]),
            tool_result("e1", "error: no path", true),
            assistant(vec![call(
                "e2",
                "read_file",
                serde_json::json!({ "path": "notes.txt" }),
            )]),
            tool_result("e2", "the answer is plumbus", false),
            assistant(vec![ContentBlock::text(
                "The file says the answer is plumbus.",
            )]),
            event(SessionEventKind::TurnEnded {
                stop: "Done".to_string(),
            }),
        ];
        let ledger = EvidenceLedger::project(&events);
        let process = extract_process(&events, &ledger);
        assert!(process.retrieval_used);
        assert_eq!(process.retrieval_count, 2);
        assert!(
            process.recovered_after_failure,
            "an errored read was followed by a grounded successful read"
        );
    }

    #[test]
    fn diff_stat_parses_unified_diff() {
        let diff = "\
diff --git a/src/lib.rs b/src/lib.rs
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,3 @@
 pub fn two() -> i32 {
-    1
+    2
+    // fixed
 }
";
        let stat = DiffStat::from_unified(diff);
        assert_eq!(stat.files, 1);
        assert_eq!(stat.added, 2);
        assert_eq!(stat.removed, 1);
    }

    #[test]
    fn quality_block_computes_vs_gold_ratio_and_reads_checks() {
        use crate::quality::{CheckOutcome, CheckStatus};
        let diff = DiffStat {
            added: 6,
            removed: 0,
            files: 1,
        };
        let gold = DiffStat {
            added: 3,
            removed: 0,
            files: 1,
        };
        let checks = vec![
            CheckOutcome {
                name: "fmt".into(),
                status: CheckStatus::Passed,
                detail: String::new(),
                fixed: false,
                severity: None,
            },
            CheckOutcome {
                name: "clippy".into(),
                status: CheckStatus::Failed,
                detail: "1 warning".into(),
                fixed: false,
                severity: None,
            },
        ];
        let quality = QualityBlock::from_signals(&diff, Some(&gold), &checks, Some(2), true);
        assert_eq!(quality.vs_gold_ratio, Some(2.0));
        assert!(quality.format_clean);
        assert!(!quality.lint_clean, "the failing clippy check is a finding");
        assert!(
            quality.typecheck_clean,
            "an absent typecheck check counts as clean"
        );
        assert!(quality.tests_added);
    }

    #[test]
    fn tests_added_and_complexity_proxies_read_the_diff() {
        let with_test = "\
diff --git a/tests/feature.rs b/tests/feature.rs
+++ b/tests/feature.rs
+#[test]
+fn it_works() {
+    if cond { assert!(true) }
+}
";
        assert!(tests_added_in_diff(with_test));
        assert!(
            complexity_delta_in_diff(with_test) >= 1,
            "the added `if` raises the complexity proxy"
        );

        let no_test = "\
diff --git a/src/lib.rs b/src/lib.rs
+++ b/src/lib.rs
+pub const N: u32 = 3;
";
        assert!(!tests_added_in_diff(no_test));
        assert_eq!(complexity_delta_in_diff(no_test), 0);
    }
}
