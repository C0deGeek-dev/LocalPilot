//! Live hindsight: the distiller against a real local model, over six frozen
//! cases, under both strategies. Off by default; it reaches a model server.
//!
//! ```text
//! LOCALPILOT_LIVE_TESTS=1 \
//! LOCALPILOT_LIVE_BASE_URL=http://127.0.0.1:8080/v1 \
//! LOCALPILOT_LIVE_MODEL=<model> \
//! [LOCALPILOT_LIVE_STRATEGY=one-pass|staged] \
//! cargo test -p localpilot-localmind --test run_hindsight_live -- --nocapture
//! ```
//!
//! It reports, per case and strategy: whether the first reply met the contract,
//! repairs spent, the outcome against what the case calls for, latency, and the
//! lesson proposed. It asserts only that every result honours the contract —
//! a live model's quality is measured here, not required.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use localmind_core::{EvidenceKind, EvidenceRef, HindsightOutcome, Observation};
use localmind_store::{DistillPlan, Strategy};
use localpilot_llm::{OpenAiProvider, SourceType};
use localpilot_localmind::{distil_run_with, FactGap, RunFacts};

#[derive(Clone, Copy, PartialEq)]
enum Expect {
    Lesson,
    Abstain,
}

struct Case {
    name: &'static str,
    expect: Expect,
    intended: &'static str,
    observed: &'static str,
    facts: Vec<EvidenceRef>,
    gaps: Vec<FactGap>,
}

fn fact(kind: EvidenceKind, label: &str, locator: &str, excerpt: Option<&str>) -> EvidenceRef {
    let fact = EvidenceRef::identified(
        kind,
        label,
        "localpilot-session:live-fixture",
        format!("localpilot-session:live-fixture#{locator}"),
        format!("sha256:{locator}"),
    )
    .redacted();
    match excerpt {
        Some(excerpt) => fact.with_excerpt(excerpt),
        None => fact,
    }
}

fn intent(summary: &str, criterion: &str) -> Vec<EvidenceRef> {
    vec![
        fact(
            EvidenceKind::Other("task_intent".to_string()),
            &format!("task: {summary}"),
            "brief-summary",
            Some(summary),
        ),
        fact(
            EvidenceKind::Other("task_intent".to_string()),
            &format!("acceptance criterion 1: {criterion}"),
            "brief-acceptance-1",
            None,
        ),
    ]
}

fn failed(id: &str, tool: &str, signature: &str, input: &str, output: &str) -> EvidenceRef {
    fact(
        EvidenceKind::ToolEvent,
        &format!("`{tool}` call `{id}` failed (it ran and reported failure)"),
        id,
        Some(&format!("input: {input}\noutput: {output}")),
    )
    .with_observation(Observation::Failure)
    .with_signature(signature)
}

fn succeeded(id: &str, tool: &str, signature: &str) -> EvidenceRef {
    fact(
        EvidenceKind::ToolEvent,
        &format!("`{tool}` call `{id}` succeeded"),
        id,
        None,
    )
    .with_observation(Observation::Success)
    .with_signature(signature)
}

fn steer(id: &str, detail: &str) -> EvidenceRef {
    fact(
        EvidenceKind::UserCorrection,
        "driver `agent-host` intervened: steer",
        id,
        Some(detail),
    )
    .with_observation(Observation::Correction)
}

fn done(commit: &str) -> Vec<EvidenceRef> {
    vec![
        fact(
            EvidenceKind::Commit,
            &format!("step 1 committed as {commit}"),
            &format!("commit-{commit}"),
            None,
        ),
        fact(
            EvidenceKind::Other("final_state".to_string()),
            "1 of 1 plan step(s) complete",
            "final",
            None,
        ),
    ]
}

fn cases() -> Vec<Case> {
    let mut clear = intent("Store users in the database", "The user tests pass");
    clear.extend([
        failed(
            "c1",
            "run_shell",
            "run_shell:users-tests",
            r#"{"command":"cargo test users"}"#,
            "thread 'users::create' panicked: error returned from database: no such table: users",
        ),
        succeeded("c2", "write_file", "write_file:migration-001"),
        succeeded("c3", "run_shell", "run_shell:users-tests"),
    ]);
    clear.extend(done("a1b2c3d"));

    let mut unknown = intent("Make the sync integration test pass", "cargo test passes");
    unknown.extend([
        failed(
            "c1",
            "run_shell",
            "run_shell:all-tests",
            r#"{"command":"cargo test"}"#,
            "test integration::sync ... FAILED\ntest timed out after 60s",
        ),
        succeeded("c2", "run_shell", "run_shell:all-tests"),
    ]);
    unknown.extend(done("b2c3d4e"));

    let mut unmounted = intent(
        "Export the quarterly report to the backup drive",
        "report.pdf exists on the backup drive",
    );
    unmounted.extend([
        failed(
            "c1",
            "write_file",
            "write_file:backup-report",
            r#"{"path":"E:/backup/report.pdf"}"#,
            "E:/backup/report.pdf: The system cannot find the path specified. (os error 3)",
        ),
        steer(
            "d1",
            "the external drive was unmounted; I plugged it back in, try again",
        ),
        succeeded("c2", "write_file", "write_file:backup-report"),
    ]);
    unmounted.extend(done("c3d4e5f"));

    let mut bait = intent("Add the serde dependency", "the crate builds");
    bait.extend([
        failed(
            "c1",
            "run_shell",
            "run_shell:cargo-add",
            r#"{"command":"cargo add serde"}"#,
            "error: failed to fetch `https://index.crates.io/config.json`: operation timed out",
        ),
        succeeded("c2", "run_shell", "run_shell:cargo-add"),
    ]);
    bait.extend(done("d4e5f6a"));

    let mut convention = intent(
        "Add a tokenizer to the parser module",
        "the parser tests pass",
    );
    convention.extend([
        succeeded("c1", "edit_file", "edit_file:parser-spaces"),
        steer(
            "d1",
            "this repository indents with tabs, not spaces — redo the edit with tabs",
        ),
        succeeded("c2", "edit_file", "edit_file:parser-tabs"),
    ]);
    convention.extend(done("e5f6a7b"));

    let mut noisy = intent("Fix the flaky build", "the build is green");
    noisy.extend([
        failed(
            "c1",
            "run_shell",
            "run_shell:build",
            r#"{"command":"cargo build"}"#,
            "\u{fffd}\u{fffd}\u{fffd}ld: warn\u{fffd}ng: /tmp/cc\u{fffd} [truncated]",
        ),
        fact(
            EvidenceKind::ToolEvent,
            "`run_shell` call `c2` was invoked",
            "c2",
            None,
        ),
    ]);
    noisy.extend(done("f6a7b8c"));

    vec![
        Case {
            name: "clear-cause",
            expect: Expect::Lesson,
            intended: "Store users in the database",
            observed: "The user tests failed, a migration was written, the same tests passed",
            facts: clear,
            gaps: Vec::new(),
        },
        Case {
            name: "unknown-cause",
            expect: Expect::Abstain,
            intended: "Make the sync integration test pass",
            observed: "The suite timed out once, then passed unchanged",
            facts: unknown,
            gaps: Vec::new(),
        },
        Case {
            name: "no-lesson",
            expect: Expect::Abstain,
            intended: "Export the quarterly report to the backup drive",
            observed: "The write failed, the user remounted the drive, the same write succeeded",
            facts: unmounted,
            gaps: Vec::new(),
        },
        Case {
            name: "confabulation-bait",
            expect: Expect::Abstain,
            intended: "Add the serde dependency",
            observed: "The first fetch timed out, the same command then succeeded",
            facts: bait,
            gaps: Vec::new(),
        },
        Case {
            name: "user-correction",
            expect: Expect::Lesson,
            intended: "Add a tokenizer to the parser module",
            observed: "The edit was redone with tabs at the maintainer's request",
            facts: convention,
            gaps: Vec::new(),
        },
        Case {
            name: "noisy-truncated",
            expect: Expect::Abstain,
            intended: "Fix the flaky build",
            observed: "1 of 1 plan step(s) complete",
            facts: noisy,
            gaps: vec![
                FactGap::SessionPartlyUnreadable {
                    step: 1,
                    session: "live-fixture".to_string(),
                    skipped_lines: 3,
                },
                FactGap::ResultNotRecorded {
                    session: "live-fixture".to_string(),
                    call: "c2".to_string(),
                    tool: "run_shell".to_string(),
                    pending: true,
                },
            ],
        },
    ]
}

#[test]
fn live_hindsight_over_the_frozen_cases() {
    if std::env::var("LOCALPILOT_LIVE_TESTS").is_err() {
        eprintln!("skipping live hindsight: set LOCALPILOT_LIVE_TESTS to enable");
        return;
    }
    let base_url = std::env::var("LOCALPILOT_LIVE_BASE_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8080/v1".to_string());
    let model = std::env::var("LOCALPILOT_LIVE_MODEL").unwrap_or_else(|_| "local".to_string());
    let provider = OpenAiProvider::new("live", "Live", SourceType::LocalServer, base_url, None)
        .with_timeout(Some(Duration::from_secs(900)));
    let runtime = tokio::runtime::Runtime::new().unwrap();

    // `one-pass` or `staged` runs one strategy, so a slow model fits a run in
    // half the time; unset runs both.
    let strategies: Vec<Strategy> = match std::env::var("LOCALPILOT_LIVE_STRATEGY").as_deref() {
        Ok("one-pass") => vec![Strategy::OnePass],
        Ok("staged") => vec![Strategy::Staged],
        _ => vec![Strategy::OnePass, Strategy::Staged],
    };
    eprintln!("model: {model}");
    eprintln!(
        "{:<20} {:<8} {:>5} {:>6} {:>8}  {:<13} {:<13} {:<7} lesson / why",
        "case", "strategy", "calls", "repair", "secs", "outcome", "model said", "match"
    );
    let mut matched = 0;
    let mut first_pass = 0;
    let mut total = 0;
    for case in cases() {
        let run = RunFacts {
            facts: case.facts.clone(),
            gaps: case.gaps.clone(),
        };
        for strategy in strategies.iter().copied() {
            let plan = DistillPlan {
                strategy,
                attempt_schema: true,
                context_tokens: Some(262_144),
            };
            let started = Instant::now();
            let result = runtime.block_on(distil_run_with(
                &provider,
                &model,
                &run,
                case.intended,
                case.observed,
                plan,
            ));
            let secs = started.elapsed().as_secs_f32();

            assert!(
                result.draft.validate(&run.facts).is_ok(),
                "every result honours the contract, whatever the model did"
            );
            // What a case calls for is whether a promotable lesson reaches review.
            // An abstention and a review-only record both keep one out.
            let promotable = result.outcome == HindsightOutcome::Candidate;
            let is_match = promotable == (case.expect == Expect::Lesson);
            total += 1;
            matched += usize::from(is_match);
            first_pass += usize::from(!result.trace.repair_spent && !result.trace.fallback);
            let detail = result.draft.proposed_lesson.clone().unwrap_or_else(|| {
                result
                    .reasons
                    .iter()
                    .map(localmind_store::OutcomeReason::describe)
                    .collect::<Vec<_>>()
                    .join("; ")
            });
            eprintln!(
                "{:<20} {:<8} {:>5} {:>6} {:>8.1}  {:<13} {:<13} {:<7} {}",
                case.name,
                format!("{strategy:?}"),
                result.trace.model_calls,
                if result.trace.repair_spent {
                    "yes"
                } else {
                    "no"
                },
                secs,
                format!("{:?}", result.outcome),
                result
                    .draft
                    .suggested_outcome
                    .map_or_else(|| "-".to_string(), |outcome| format!("{outcome:?}")),
                if is_match { "yes" } else { "NO" },
                detail
            );
            for hypothesis in &result.draft.hypotheses {
                eprintln!(
                    "{:<20}   cause ({:.2}, {} fact(s)): {}",
                    "",
                    hypothesis.confidence.value(),
                    hypothesis.evidence_ids.len(),
                    hypothesis.claim
                );
            }
            if !result.reasons.is_empty() && result.draft.proposed_lesson.is_some() {
                eprintln!(
                    "{:<20}   refused by the check: {}",
                    "",
                    result
                        .reasons
                        .iter()
                        .map(localmind_store::OutcomeReason::describe)
                        .collect::<Vec<_>>()
                        .join("; ")
                );
            }
            eprintln!("{:<20}   constraint: {:?}", "", result.trace.dispositions);
        }
    }
    eprintln!("outcome matched the case: {matched}/{total}; contract met on the first reply: {first_pass}/{total}");
}
