//! Scorecard derivation from a headless run's artefacts.
//!
//! The scorecard *contract* — the three-layer shape both corpora honour — lives
//! in `localx_eval_core::scorecard`; this module owns the producer side that
//! only this crate can supply: deriving the `process` and `speed` blocks from
//! the session event log via the [`EvidenceLedger`] projection, and assembling
//! a full [`Scorecard`] from one completed run. The one-line
//! [`DisciplineMetrics::scorecard_line`] text scorecard stays for back-compat
//! with the existing live-model pipeline; the structured card is the superset.
//!
//! Everything here is deterministic — no model in the loop — so the same inputs
//! always yield the same blocks and the derivation is safe for offline CI.

use localpilot_store::{SessionEvent, SessionEventKind};
use localx_eval_core::{
    complexity_delta_in_diff, tests_added_in_diff, DiffStat, DisciplineMetrics, ProcessBlock,
    QualityBlock, ResultsBlock, Scorecard, SpeedBlock, SCORECARD_SCHEMA,
};

use crate::evidence::{CallOutcome, EvidenceLedger};
use crate::quality::CheckOutcome;

/// A borrowed validator mapping a call's `(tool, input)` to `Some(valid)` when
/// the tool is known and checkable, or `None` for an unknown/MCP tool.
pub type SchemaValidator<'a> = &'a dyn Fn(&str, &serde_json::Value) -> Option<bool>;

/// The artefacts of one completed headless run, from which [`build_scorecard`]
/// assembles a [`Scorecard`]. The caller supplies the graded `results` and the
/// captured `diff_text`; the `quality`, `process`, and `speed` blocks are derived
/// here from the diff and the session event trace. `judge` is left `None` — a
/// caller that runs the LLM-as-judge attaches it afterward.
pub struct RunInputs<'a> {
    /// Task identifier.
    pub task: String,
    /// The harness arm this run used.
    pub arm: String,
    /// The model id (or `fake`).
    pub model: String,
    /// The graded results block (the runner/grader supplies the verdict).
    pub results: ResultsBlock,
    /// The produced unified diff (`git diff`).
    pub diff_text: &'a str,
    /// The gold diff, for the vs-gold ratio; `None` when there is no gold patch.
    pub gold: Option<DiffStat>,
    /// The quality-gate check outcomes, if the gate ran (empty otherwise).
    pub gate: &'a [CheckOutcome],
    /// The run's session event trace.
    pub events: &'a [SessionEvent],
    /// Runner-measured wall-clock duration, in milliseconds.
    pub wall_ms: u64,
    /// Optional schema validator that fills `schema_valid` on each projected
    /// call (mapping a call's `(tool, input)` to `Some(valid)`, or `None` for an
    /// unknown/MCP tool) so the process block can report `schema_valid_rate`.
    /// `None` leaves the validity metric dormant (`process.discipline = None`),
    /// reproducing the prior behaviour exactly.
    pub schema_validator: Option<SchemaValidator<'a>>,
}

/// Assemble a [`Scorecard`] from one completed run's artefacts — the shared
/// derivation the `eval` CLI uses to emit a scorecard from a headless run, so the
/// quality/process blocks are computed identically to the eval corpora.
#[must_use]
pub fn build_scorecard(inputs: RunInputs) -> Scorecard {
    let diff = DiffStat::from_unified(inputs.diff_text);
    let mut ledger = EvidenceLedger::project(inputs.events);
    // When the caller supplies the tool schemas, light up the validity metric:
    // fill `schema_valid` on each call and attach the per-run discipline rates so
    // the scorecard reports `schema_valid_rate`. With no validator the block stays
    // `None`, exactly as before.
    let discipline = inputs.schema_validator.map(|validate| {
        ledger.fill_schema_validity(validate);
        single_run_discipline(&ledger)
    });
    let mut process = extract_process(inputs.events, &ledger);
    process.discipline = discipline;
    Scorecard {
        schema: SCORECARD_SCHEMA,
        task: inputs.task,
        arm: inputs.arm,
        model: inputs.model,
        results: inputs.results,
        quality: QualityBlock::from_signals(
            &diff,
            inputs.gold.as_ref(),
            inputs.gate,
            Some(complexity_delta_in_diff(inputs.diff_text)),
            tests_added_in_diff(inputs.diff_text),
        ),
        process,
        speed: speed_from_events(inputs.events, inputs.wall_ms),
        judge: None,
    }
}

/// `numerator / denominator`, or `default` when nothing applies.
fn ratio(numerator: usize, denominator: usize, default: f64) -> f64 {
    if denominator == 0 {
        default
    } else {
        numerator as f64 / denominator as f64
    }
}

/// The per-capability discipline rates derivable from a single run's evidence
/// ledger, with `schema_valid_rate` the headline. The per-call rates (schema
/// validity, first-call accuracy, redundancy, recovery) are real; the
/// cross-scenario-only rates (required-tool usage, selection precision, the claim
/// violations) have no meaning for one task and take their vacuous-best value.
/// `schema_valid` must already be filled (via [`EvidenceLedger::fill_schema_validity`]).
#[must_use]
pub fn single_run_discipline(ledger: &EvidenceLedger) -> DisciplineMetrics {
    let calls = ledger.calls();

    let mut schema = (0usize, 0usize);
    for call in calls {
        if let Some(valid) = call.schema_valid {
            schema.1 += 1;
            if valid {
                schema.0 += 1;
            }
        }
    }

    let first_call_arg_accuracy = match calls.first().map(|c| c.schema_valid) {
        Some(Some(false)) => 0.0,
        _ => 1.0, // valid, unknown, or no first call: vacuous-best
    };

    let mut redundant = 0usize;
    for (i, call) in calls.iter().enumerate() {
        if calls[..i]
            .iter()
            .any(|earlier| earlier.name == call.name && earlier.input == call.input)
        {
            redundant += 1;
        }
    }

    let any_error = calls.iter().any(|c| c.outcome == CallOutcome::Error);
    let recovered = match calls.iter().position(|c| c.outcome == CallOutcome::Error) {
        Some(idx) => calls[idx + 1..]
            .iter()
            .any(|c| c.outcome == CallOutcome::Ok && c.claim_referenced),
        None => false,
    };

    DisciplineMetrics {
        scenarios: 1,
        required_tool_usage: 1.0,
        tool_selection_precision: 1.0,
        schema_valid_rate: ratio(schema.0, schema.1, 1.0),
        first_call_arg_accuracy,
        recovery_success: if any_error {
            f64::from(u8::from(recovered))
        } else {
            1.0
        },
        unsupported_claim_rate: 0.0,
        false_success_rate: 0.0,
        redundant_call_rate: ratio(redundant, calls.len(), 0.0),
        avg_calls_per_success: calls.len() as f64,
    }
}

/// Sum the token usage reported in the event log into a [`SpeedBlock`];
/// `wall_ms` is the runner's own measurement and is supplied separately.
#[must_use]
pub fn speed_from_events(events: &[SessionEvent], wall_ms: u64) -> SpeedBlock {
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
    SpeedBlock {
        wall_ms,
        input_tokens,
        output_tokens,
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

    // Interventions: external-driver corrections (steers, cancellations,
    // permission replies) recorded on the durable event log by `mcp serve`.
    let interventions = events
        .iter()
        .filter(|e| matches!(e.kind, SessionEventKind::DriverIntervention { .. }))
        .count() as u32;

    // Exit reason: the last recorded turn stop label.
    let exit_reason = events
        .iter()
        .rev()
        .find_map(|e| match &e.kind {
            SessionEventKind::TurnEnded { stop, .. } => Some(stop.clone()),
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
        interventions,
        discipline: None,
    }
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
                detail: None,
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
        assert_eq!(process.interventions, 0, "an undriven run reports zero");
    }

    #[test]
    fn process_extractor_counts_driver_interventions() {
        let events = vec![
            event(SessionEventKind::DriverIntervention {
                action: "steer".to_string(),
                detail: "prefer the existing helper".to_string(),
                activity: None,
                client: "coach".to_string(),
            }),
            event(SessionEventKind::DriverIntervention {
                action: "deny".to_string(),
                detail: "run_command: rm".to_string(),
                activity: Some("run_command".to_string()),
                client: "coach".to_string(),
            }),
            event(SessionEventKind::TurnEnded {
                stop: "Done".to_string(),
                detail: None,
            }),
        ];
        let ledger = EvidenceLedger::project(&events);
        let process = extract_process(&events, &ledger);
        assert_eq!(process.interventions, 2);
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
                detail: None,
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
    fn build_scorecard_assembles_from_run_artefacts() {
        let events = vec![
            assistant(vec![call(
                "c1",
                "write_file",
                serde_json::json!({ "path": "a.rs", "content": "fn a() {}" }),
            )]),
            tool_result("c1", "ok", false),
            event(SessionEventKind::UsageReported {
                input_tokens: 12,
                output_tokens: 4,
            }),
            event(SessionEventKind::TurnEnded {
                stop: "Done".to_string(),
                detail: None,
            }),
        ];
        let diff = "diff --git a/a.rs b/a.rs\n+++ b/a.rs\n+fn a() {}\n";
        let card = build_scorecard(RunInputs {
            task: "t1".to_string(),
            arm: "full".to_string(),
            model: "fake".to_string(),
            results: ResultsBlock {
                passed: true,
                regression_safe: true,
                partial_credit: 1.0,
                tests_total: 1,
                tests_passed: 1,
            },
            diff_text: diff,
            gold: Some(DiffStat {
                added: 1,
                removed: 0,
                files: 1,
            }),
            gate: &[],
            events: &events,
            wall_ms: 250,
            schema_validator: None,
        });
        assert_eq!(card.task, "t1");
        assert_eq!(card.quality.diff_added, 1);
        assert_eq!(card.quality.vs_gold_ratio, Some(1.0));
        assert_eq!(card.process.tool_calls, 1);
        assert_eq!(card.process.exit_reason, "Done");
        assert_eq!(card.speed.input_tokens, 12);
        assert!(card.judge.is_none());
        assert!(
            card.process.discipline.is_none(),
            "with no validator the validity metric stays dormant"
        );
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn a_schema_validator_lights_up_the_validity_rate_in_the_scorecard() {
        // One valid write_file call, one invalid (missing both required fields).
        let events = vec![
            assistant(vec![call(
                "c1",
                "write_file",
                serde_json::json!({ "path": "a.rs", "content": "x" }),
            )]),
            tool_result("c1", "ok", false),
            assistant(vec![call("c2", "write_file", serde_json::json!({}))]),
            tool_result("c2", "invalid input: missing field `path`", true),
            event(SessionEventKind::TurnEnded {
                stop: "Done".to_string(),
                detail: None,
            }),
        ];
        // A toy validator: write_file requires `path` and `content`.
        let validate = |name: &str, input: &serde_json::Value| -> Option<bool> {
            (name == "write_file")
                .then(|| input.get("path").is_some() && input.get("content").is_some())
        };
        let card = build_scorecard(RunInputs {
            task: "t".to_string(),
            arm: "baseline".to_string(),
            model: "fake".to_string(),
            results: ResultsBlock {
                passed: false,
                regression_safe: true,
                partial_credit: 0.0,
                tests_total: 0,
                tests_passed: 0,
            },
            diff_text: "",
            gold: None,
            gate: &[],
            events: &events,
            wall_ms: 0,
            schema_validator: Some(&validate),
        });
        let discipline = card
            .process
            .discipline
            .expect("a validator attaches the discipline rates");
        // One of two calls validated ⇒ 0.5; the rate appears in the JSON too.
        assert!((discipline.schema_valid_rate - 0.5).abs() < f64::EPSILON);
        let value: serde_json::Value =
            serde_json::from_str(&card.to_json().expect("serialize")).expect("parse");
        assert!(value["process"]["discipline"]["schema_valid_rate"].is_number());
    }
}
