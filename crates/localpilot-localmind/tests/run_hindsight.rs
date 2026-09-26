//! Hindsight over a finished run, driven through the harness provider and
//! offered to review.
//!
//! Every provider here is scripted: these tests prove what is sent, what is
//! accepted, how many calls are made and what reaches the queue — not how well
//! any model fills the contract.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use localmind_core::HindsightOutcome;
use localpilot_core::{ContentBlock, Message, Role, SessionId, ToolCall, ToolResult};
use localpilot_harness::{Brief, BriefRevision};
use localpilot_llm::{FakeProvider, ModelProvider};
use localpilot_localmind::{capture_run_facts, offer_run_hindsight, RunFacts};
use localpilot_store::{MessageOrigin, SessionEventKind, Store};
use serde_json::json;

const BRIEF: &str = "# Brief: users\n\n## Summary\n\nStore users in the database.\n\n\
## Requirements\n\n- A users table\n\n## Constraints\n\n- Keep it small\n\n\
## Non-Goals\n\n- Accounts\n\n## Acceptance Criteria\n\n- The user tests pass\n";

const LESSON: &str = "Write the schema migration before the test that reads its table";

/// A finished one-step run: the tests failed on a missing table, a migration
/// was written, the same tests passed.
fn finished_run(learning: &str) -> tempfile::TempDir {
    finished_run_failing_with(learning, "no such table: users")
}

/// [`finished_run`] with the failing test run's output given.
fn finished_run_failing_with(learning: &str, failure: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("brief.md"), BRIEF).unwrap();
    std::fs::write(root.join(".localmind.toml"), learning).unwrap();
    let session = SessionId::new();
    let revision = BriefRevision::of(&Brief::parse(BRIEF).unwrap());
    std::fs::write(
        root.join("PROGRESS.md"),
        format!(
            "# Progress: users\nBranch: feature/users\nBrief: {revision}\n\n## Steps\n\n\
- [x] 1. Add the users table\n  - commit: abc1234\n  - attempts: 1\n  - sessions: {session}\n"
        ),
    )
    .unwrap();
    let store = Store::open(root);
    let push = |kind| {
        store.append_event(session, None, kind).unwrap();
    };
    push(SessionEventKind::StepStarted {
        number: 1,
        description: "Add the users table".to_string(),
    });
    for (id, name, input, output, is_error) in [
        (
            "c1",
            "run_shell",
            json!({ "command": "cargo test users" }),
            failure,
            true,
        ),
        (
            "c2",
            "write_file",
            json!({ "path": "migrations/001.sql" }),
            "wrote 40 bytes",
            false,
        ),
        (
            "c3",
            "run_shell",
            json!({ "command": "cargo test users" }),
            "3 passed",
            false,
        ),
    ] {
        push(SessionEventKind::Message {
            message: Message::new(
                Role::Assistant,
                vec![ContentBlock::ToolUse(ToolCall::new(id.into(), name, input))],
            ),
            origin: MessageOrigin::Assistant,
        });
        let result = if is_error {
            ToolResult::error(id.into(), output)
        } else {
            ToolResult::success(id.into(), output)
        };
        push(SessionEventKind::Message {
            message: Message::new(Role::Tool, vec![ContentBlock::ToolResult(result)]),
            origin: MessageOrigin::ToolResult,
        });
    }
    push(SessionEventKind::TurnEnded {
        stop: "Done".to_string(),
        detail: None,
    });
    dir
}

const LEARNING: &str = "[learning]\nenabled = true\nallowed_scopes = [\"project\"]\n";

fn id_of(run: &RunFacts, label: &str) -> String {
    run.facts
        .iter()
        .find(|fact| fact.label.starts_with(label))
        .unwrap_or_else(|| panic!("no fact labelled {label}"))
        .id
        .as_str()
        .to_string()
}

fn draft(run: &RunFacts, lesson: Option<&str>) -> String {
    json!({
        "version": 1,
        "intended_outcome": "Store users in the database",
        "observed_outcome": "The tests failed on a missing table, then passed after a migration",
        "hypotheses": [{
            "claim": "The users table did not exist because its migration had not been written",
            "evidence_ids": [id_of(run, "`run_shell` call `c1` failed"), id_of(run, "`write_file` call `c2`")],
            "confidence": 0.7
        }],
        "intervention": "Write the migration first",
        "proposed_lesson": lesson,
        "suggested_outcome": "Candidate"
    })
    .to_string()
}

/// A provider with a large declared context, so the one-pass strategy runs.
fn one_pass(replies: &[&str]) -> FakeProvider {
    replies.iter().fold(
        FakeProvider::new().declaring_context_tokens(131_072),
        |provider, reply| provider.text(reply),
    )
}

struct Queued {
    summary: String,
    candidate: serde_json::Value,
}

fn queued(root: &Path) -> Vec<Queued> {
    let database = root.join(".localmind").join("localmind.sqlite");
    if !database.exists() {
        return Vec::new();
    }
    let connection = rusqlite::Connection::open(database).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT candidate_json FROM review_items WHERE session_id = 'completion-retrospective'",
        )
        .unwrap();
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|json| {
            let candidate: serde_json::Value = serde_json::from_str(&json.unwrap()).unwrap();
            Queued {
                summary: candidate["summary"].as_str().unwrap().to_string(),
                candidate,
            }
        })
        .collect()
}

async fn offer(
    root: &Path,
    provider: &dyn ModelProvider,
    run: &RunFacts,
) -> localpilot_localmind::HindsightOffer {
    offer_run_hindsight(
        root,
        provider,
        "m",
        run,
        "users",
        "Store users in the database",
        "1 of 1 plan step(s) complete",
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn an_earned_lesson_reaches_review_carrying_its_draft_and_facts() {
    let dir = finished_run(LEARNING);
    let root = dir.path();
    let run = capture_run_facts(root, &Store::open(root));
    let provider = one_pass(&[&draft(&run, Some(LESSON))]);

    let offer = offer(root, &provider, &run).await;

    assert_eq!(offer.distillation.outcome, HindsightOutcome::Candidate);
    assert_eq!(offer.lesson.as_deref(), Some(LESSON));
    assert!(offer.enqueued.is_some());
    assert_eq!(provider.requests().len(), 1);
    let queued = queued(root);
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].summary, LESSON);
    let candidate = &queued[0].candidate;
    assert_eq!(candidate["suggested_action"], "PromoteToMemory");
    assert!(
        candidate["hindsight"]["hypotheses"]
            .as_array()
            .unwrap()
            .len()
            == 1
    );
    let evidence = candidate["evidence"].as_array().unwrap();
    assert_eq!(
        evidence.len(),
        run.facts.len() + 1,
        "the facts plus the origin"
    );
    assert!(
        evidence
            .iter()
            .all(|fact| fact["id"].as_str().unwrap().starts_with("ev-")),
        "every id is verifiable, the origin's too"
    );
    assert!(candidate["evidence_text"]
        .as_str()
        .unwrap()
        .starts_with("Hindsight: Candidate — 1 model call(s)"));
}

#[tokio::test]
async fn the_same_run_offered_twice_is_one_pending_row() {
    let dir = finished_run(LEARNING);
    let root = dir.path();
    let run = capture_run_facts(root, &Store::open(root));
    let reply = draft(&run, Some(LESSON));

    let first = offer(root, &one_pass(&[&reply]), &run).await;
    let second = offer(root, &one_pass(&[&reply]), &run).await;

    assert!(first.enqueued.is_some());
    assert!(
        second.enqueued.is_none(),
        "a restatement merges into the pending row"
    );
    let queued = queued(root);
    assert_eq!(queued.len(), 1);
    // Logic ran both times and agreed; the lesson holds the result once.
    assert_eq!(first.logic[0].identity(), second.logic[0].identity());
    assert_eq!(
        queued[0].candidate["experiments"].as_array().unwrap().len(),
        1
    );
}

#[tokio::test]
async fn an_abstention_queues_nothing_unless_the_project_asks() {
    for (learning, expect_record) in [
        (LEARNING.to_string(), false),
        (
            format!("{LEARNING}\n[review]\nrecord_abstentions = true\n"),
            true,
        ),
    ] {
        let dir = finished_run(&learning);
        let root = dir.path();
        let run = capture_run_facts(root, &Store::open(root));
        // A cause, and nothing reusable proposed from it.
        let provider = one_pass(&[&draft(&run, None)]);

        let offer = offer(root, &provider, &run).await;

        assert_eq!(offer.distillation.outcome, HindsightOutcome::NoLesson);
        assert_eq!(offer.lesson, None, "an abstention never reaches LESSONS.md");
        let queued = queued(root);
        if expect_record {
            assert_eq!(queued.len(), 1);
            assert!(queued[0]
                .summary
                .starts_with("Hindsight on `users` found no lesson"));
            assert_eq!(queued[0].candidate["requires_edit_before_promotion"], true);
            assert_eq!(queued[0].candidate["suggested_action"], "KeepForSession");
        } else {
            assert!(queued.is_empty());
        }
    }
}

#[tokio::test]
async fn a_try_again_lesson_is_refused_by_the_check_not_by_the_model() {
    let dir = finished_run(LEARNING);
    let root = dir.path();
    let run = capture_run_facts(root, &Store::open(root));
    let provider = one_pass(&[&draft(&run, Some("Retry the test run when it fails"))]);

    let offer = offer(root, &provider, &run).await;

    assert_eq!(
        offer.distillation.draft.suggested_outcome,
        Some(HindsightOutcome::Candidate)
    );
    assert_eq!(offer.distillation.outcome, HindsightOutcome::NoLesson);
    assert!(queued(root).is_empty());
}

#[tokio::test]
async fn an_unreachable_model_is_recorded_for_review_with_the_facts_and_no_cause() {
    let dir = finished_run(LEARNING);
    let root = dir.path();
    let run = capture_run_facts(root, &Store::open(root));
    let provider = FakeProvider::new()
        .declaring_context_tokens(131_072)
        .fail_open(1);

    let offer = offer(root, &provider, &run).await;

    assert_eq!(offer.distillation.outcome, HindsightOutcome::NeedsReview);
    assert_eq!(offer.distillation.trace.model_calls, 0);
    let queued = queued(root);
    assert_eq!(queued.len(), 1);
    assert!(queued[0]
        .summary
        .starts_with("Hindsight on `users` needs review: no analysis ran"));
    assert_eq!(queued[0].candidate["requires_edit_before_promotion"], true);
    assert!(queued[0].candidate["hindsight"]["hypotheses"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        queued[0].candidate["evidence"].as_array().unwrap().len(),
        run.facts.len() + 1,
        "the facts are kept"
    );
}

#[tokio::test]
async fn a_reply_that_keeps_breaking_the_contract_gets_one_repair_and_no_more() {
    let dir = finished_run(LEARNING);
    let root = dir.path();
    let run = capture_run_facts(root, &Store::open(root));
    let invented =
        draft(&run, Some(LESSON)).replace(&id_of(&run, "`write_file` call `c2`"), "ev-0000");
    let provider = one_pass(&[&invented, &invented]);

    let offer = offer(root, &provider, &run).await;

    assert_eq!(offer.distillation.outcome, HindsightOutcome::Malformed);
    assert_eq!(provider.requests().len(), 2);
    assert!(queued(root)[0].summary.contains("needs review"));
}

#[tokio::test]
async fn a_refused_schema_is_reported_and_not_sent_again() {
    let dir = finished_run(LEARNING);
    let root = dir.path();
    let run = capture_run_facts(root, &Store::open(root));
    // A small declared context: the staged passes run, so two requests go out.
    let causes = json!({
        "intended_outcome": "Store users in the database",
        "observed_outcome": "The tests passed after a migration",
        "hypotheses": [{
            "claim": "The migration had not been written",
            "evidence_ids": [id_of(&run, "`run_shell` call `c1` failed"), id_of(&run, "`write_file` call `c2`")],
            "confidence": 0.7
        }]
    })
    .to_string();
    let proposal = json!({ "proposed_lesson": LESSON }).to_string();
    let provider = FakeProvider::new()
        .declaring_context_tokens(8_192)
        .declaring_constrained_decoding()
        .refusing_constraints()
        .text(&causes)
        .text(&proposal);

    let offer = offer(root, &provider, &run).await;

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0].tool_constraint.is_some(),
        "the schema was attempted"
    );
    assert!(
        requests[1].tool_constraint.is_none(),
        "and not sent again once refused"
    );
    assert_eq!(
        offer.distillation.trace.dispositions,
        vec![
            localmind_inference::ConstraintDisposition::RefusedByTransport,
            localmind_inference::ConstraintDisposition::NotRequested
        ]
    );
    assert!(!offer.distillation.trace.repair_spent);
    assert_eq!(offer.distillation.outcome, HindsightOutcome::Candidate);
    assert!(queued(root)[0].candidate["evidence_text"]
        .as_str()
        .unwrap()
        .contains("the server refused the output schema"));
}

#[tokio::test]
async fn a_project_with_learning_off_spends_no_model_call() {
    let dir = finished_run("[learning]\nenabled = false\n");
    let root = dir.path();
    let run = capture_run_facts(root, &Store::open(root));
    let provider = one_pass(&[&draft(&run, Some(LESSON))]);

    let result = offer_run_hindsight(root, &provider, "m", &run, "users", "i", "o").await;

    assert!(result.is_err());
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn the_facts_say_what_repeated_so_the_check_can_tell_a_fix_from_a_retry() {
    let dir = finished_run(LEARNING);
    let root = dir.path();
    let run = capture_run_facts(root, &Store::open(root));
    let fact = |label: &str| {
        run.facts
            .iter()
            .find(|fact| fact.label.starts_with(label))
            .unwrap()
            .clone()
    };

    let failed = fact("`run_shell` call `c1` failed");
    let passed = fact("`run_shell` call `c3` succeeded");
    let wrote = fact("`write_file` call `c2` succeeded");
    assert_eq!(
        failed.observation(),
        Some(localmind_core::Observation::Failure)
    );
    assert_eq!(
        passed.observation(),
        Some(localmind_core::Observation::Success)
    );
    assert_eq!(
        failed.signature(),
        passed.signature(),
        "the same command is the same attempt"
    );
    assert_ne!(failed.signature(), wrote.signature());
    assert!(
        failed.identity_is_intact(),
        "marks are metadata, not identity"
    );
}

#[tokio::test]
async fn a_lesson_over_a_damaged_log_is_kept_for_review_not_queued_as_a_lesson() {
    let dir = finished_run(LEARNING);
    let root = dir.path();
    let sessions = root.join(".localpilot").join("sessions");
    let log = std::fs::read_dir(&sessions)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.to_string_lossy().ends_with(".events.jsonl"))
        .unwrap();
    let mut text = std::fs::read_to_string(&log).unwrap();
    text.push_str("{\"v\":1,\"id\":\"trunc");
    std::fs::write(&log, text).unwrap();
    let run = capture_run_facts(root, &Store::open(root));
    let provider = one_pass(&[&draft(&run, Some(LESSON))]);

    let offer = offer(root, &provider, &run).await;

    assert_eq!(offer.distillation.outcome, HindsightOutcome::NeedsReview);
    assert_eq!(offer.lesson, None, "nothing reaches LESSONS.md");
    let queued = queued(root);
    assert_eq!(queued.len(), 1);
    assert!(
        queued[0].summary.contains("incomplete record"),
        "{}",
        queued[0].summary
    );
    assert_eq!(queued[0].candidate["requires_edit_before_promotion"], true);
    assert_eq!(
        queued[0].candidate["hindsight"]["proposed_lesson"], LESSON,
        "the proposal is kept for a person to judge"
    );
}

#[tokio::test]
async fn an_earned_lesson_is_classified_and_its_frozen_assignment_is_kept() {
    let dir = finished_run(LEARNING);
    let root = dir.path();
    let run = capture_run_facts(root, &Store::open(root));
    let provider = one_pass(&[&draft(&run, Some(LESSON))]);

    let offer = offer(root, &provider, &run).await;

    let lab = offer
        .lab
        .expect("a lesson that reached review is classified");
    assert_eq!(
        lab.eligibility,
        localpilot_localmind::Eligibility::Logic,
        "{lab:?}"
    );
    let record = root
        .join(".localpilot")
        .join("lab")
        .join("assignments")
        .join(format!("{}.json", lab.candidate_identity));
    let kept: localpilot_localmind::LabClassification =
        serde_json::from_str(&std::fs::read_to_string(&record).unwrap()).unwrap();
    assert_eq!(kept, lab, "the record is the frozen form");
    // Logic ran at completion, over the run's own recorded trajectory, and its
    // result is on the lesson in review.
    assert_eq!(offer.logic.len(), 1);
    let logic = &offer.logic[0];
    assert_eq!(logic.tier, localmind_core::EvidenceTier::Logic);
    assert_eq!(
        logic.verdict,
        localmind_core::LabVerdict::Valid,
        "{logic:?}"
    );
    assert_eq!(
        logic.inputs.assignment_identity.as_deref(),
        Some(lab.assignments[0].identity().as_str())
    );
    let queued = queued(root);
    assert_eq!(queued.len(), 1, "the result merged into the pending row");
    let stored: localmind_core::CandidateLesson =
        serde_json::from_value(queued[0].candidate.clone()).unwrap();
    assert_eq!(stored.experiments, vec![logic.clone()]);
    logic
        .validate(&stored)
        .expect("the stored result is bound to the stored lesson");
    assert_eq!(
        queued[0].candidate["suggested_action"], "PromoteToMemory",
        "a Valid Logic result changes nothing about review"
    );
}

#[tokio::test]
async fn nothing_the_logic_run_stores_carries_what_capture_redacted() {
    // The failing run printed a path the project marked sensitive. The event
    // store does not know LocalMind's configuration, so the log keeps it.
    let learning = "[learning]
enabled = true
allowed_scopes = [\"project\"]
excluded_paths = [\"internal/acquisition\"]
";
    let dir = finished_run_failing_with(
        learning,
        "internal/acquisition/schema.sql: no such table: users",
    );
    let root = dir.path();
    let logged: String = std::fs::read_dir(root.join(".localpilot").join("sessions"))
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect();
    assert!(
        logged.contains("internal/acquisition"),
        "precondition: the raw log carries the path"
    );
    let run = capture_run_facts(root, &Store::open(root));
    let provider = one_pass(&[&draft(&run, Some(LESSON))]);

    let offer = offer(root, &provider, &run).await;

    assert_eq!(offer.logic.len(), 1);
    assert_eq!(offer.logic[0].verdict, localmind_core::LabVerdict::Valid);
    let queued = queued(root);
    let stored = queued[0].candidate.to_string();
    assert!(
        stored.contains("\"experiments\""),
        "the Logic result is in review: {stored}"
    );
    assert!(!stored.contains("internal/acquisition"), "{stored}");
    assert_eq!(queued[0].summary, LESSON);
}

#[tokio::test]
async fn a_lesson_no_test_can_judge_says_why_in_review_and_keeps_its_review_path() {
    let dir = finished_run(LEARNING);
    let root = dir.path();
    let run = capture_run_facts(root, &Store::open(root));
    let preference = "Prefer writing migrations as plain SQL files in this repository";
    let provider = one_pass(&[&draft(&run, Some(preference))]);

    let offer = offer(root, &provider, &run).await;

    let lab = offer.lab.unwrap();
    assert_eq!(
        lab.eligibility,
        localpilot_localmind::Eligibility::NotExecutable
    );
    let queued = queued(root);
    assert_eq!(queued.len(), 1, "the result merged into the pending row");
    assert_eq!(queued[0].summary, preference);
    let experiments = queued[0].candidate["experiments"].as_array().unwrap();
    assert_eq!(experiments.len(), 1);
    assert_eq!(experiments[0]["verdict"], "NotExecutable");
    assert_eq!(experiments[0]["reasons"], serde_json::json!(["Preference"]));
    assert_eq!(
        experiments[0]["inputs"]["candidate_identity"],
        lab.candidate_identity
    );
    assert_eq!(
        queued[0].candidate["suggested_action"], "PromoteToMemory",
        "the ordinary review path is unchanged"
    );
}
