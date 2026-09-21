//! Drafting a brief without committing to it, and committing to it exactly once.
//!
//! The property under test throughout is that nothing reaches the project until
//! someone approves it: a draft, a revision, a failed attempt, and an exhausted
//! repair budget all leave the workspace byte-identical.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;

use localpilot_harness::{
    draft_brief, persist_approved, revise_brief, Approval, Brief, BriefDraft, DraftOutcome,
    GuidanceParams, GuidanceRecord,
};
use localpilot_llm::FakeProvider;

const BRIEF: &str = "# Brief: greeting\n\n## Summary\n\nGreet the user.\n\n\
## Requirements\n\n- It greets\n\n## Constraints\n\n- Be small\n\n\
## Non-Goals\n\n- Anything else\n\n## Acceptance Criteria\n\n- A test passes\n";

const REVISED: &str = "# Brief: greeting\n\n## Summary\n\nGreet the user.\n\n\
## Requirements\n\n- It greets\n- It greets in two languages\n\n\
## Constraints\n\n- Be small\n\n## Non-Goals\n\n- Anything else\n\n\
## Acceptance Criteria\n\n- A test passes\n";

/// A guidance reply that scores the idea as fully settled, so drafting proceeds.
const SETTLED: &str = r#"{"axes": []}"#;

/// A guidance reply with one open decision, so the gate asks.
const OPEN_AXIS: &str = r#"{"axes": [
  {"axis": "storage", "resolved": false, "evidence": "not specified",
   "question": "Where should greetings be stored?"}
]}"#;

/// Every file in `root`, with its bytes and modification time.
fn snapshot(root: &Path) -> BTreeMap<String, (Vec<u8>, Option<SystemTime>)> {
    let mut out = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let metadata = entry.metadata().unwrap();
        if metadata.is_file() {
            out.insert(
                entry.file_name().to_string_lossy().into_owned(),
                (
                    std::fs::read(entry.path()).unwrap(),
                    metadata.modified().ok(),
                ),
            );
        }
    }
    out
}

fn draft_of(brief_text: &str) -> BriefDraft {
    BriefDraft {
        brief: Brief::parse(brief_text).unwrap(),
        idea: "greet the user".to_string(),
        model_input: "greet the user".to_string(),
        guidance: GuidanceRecord::none(),
        revisions: 0,
    }
}

#[tokio::test]
async fn drafting_produces_a_brief_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let before = snapshot(dir.path());

    let provider = FakeProvider::new().text(BRIEF);
    let outcome = draft_brief(&provider, "m", "greet the user", None)
        .await
        .unwrap();

    let DraftOutcome::Drafted(draft) = outcome else {
        panic!("expected a draft");
    };
    assert_eq!(draft.brief.name, "greeting");
    assert_eq!(draft.revisions, 0);
    assert_eq!(
        snapshot(dir.path()),
        before,
        "generation must not touch the project"
    );
}

#[tokio::test]
async fn a_settled_idea_drafts_and_an_open_one_asks() {
    let gate = GuidanceParams {
        threshold: 0.7,
        max_questions: 3,
    };

    let settled = FakeProvider::new().text(SETTLED).text(BRIEF);
    assert!(matches!(
        draft_brief(&settled, "m", "greet the user", Some(gate))
            .await
            .unwrap(),
        DraftOutcome::Drafted(_)
    ));

    let open = FakeProvider::new().text(OPEN_AXIS);
    match draft_brief(&open, "m", "greet the user", Some(gate))
        .await
        .unwrap()
    {
        DraftOutcome::NeedsGuidance {
            open, questions, ..
        } => {
            assert_eq!(open.len(), 1);
            assert_eq!(questions.len(), 1);
            assert!(questions[0].contains("stored"), "{questions:?}");
        }
        DraftOutcome::Drafted(_) => panic!("an unsettled idea must not draft straight through"),
    }
}

#[tokio::test]
async fn the_question_cap_is_respected_and_floored_at_one() {
    let many = r#"{"axes": [
      {"axis": "a", "resolved": false, "evidence": "not specified", "question": "A?"},
      {"axis": "b", "resolved": false, "evidence": "not specified", "question": "B?"},
      {"axis": "c", "resolved": false, "evidence": "not specified", "question": "C?"}
    ]}"#;

    for (cap, expected) in [(2, 2), (0, 1)] {
        let provider = FakeProvider::new().text(many);
        let gate = GuidanceParams {
            threshold: 0.9,
            max_questions: cap,
        };
        match draft_brief(&provider, "m", "an idea", Some(gate))
            .await
            .unwrap()
        {
            DraftOutcome::NeedsGuidance { questions, .. } => {
                assert_eq!(questions.len(), expected, "cap {cap}");
            }
            DraftOutcome::Drafted(_) => panic!("expected the gate to ask"),
        }
    }
}

#[tokio::test]
async fn a_revision_changes_the_brief_and_still_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let before = snapshot(dir.path());

    let provider = FakeProvider::new().text(REVISED);
    let revised = revise_brief(
        &provider,
        "m",
        &draft_of(BRIEF),
        "add a second language requirement",
    )
    .await
    .unwrap();

    assert_eq!(revised.requirements_len(), 2);
    assert_eq!(revised.revisions, 1, "a revision counts");
    assert_eq!(
        snapshot(dir.path()),
        before,
        "revising must not touch the project"
    );
}

trait RequirementCount {
    fn requirements_len(&self) -> usize;
}

impl RequirementCount for BriefDraft {
    fn requirements_len(&self) -> usize {
        self.brief.requirements.len()
    }
}

#[tokio::test]
async fn an_exhausted_repair_budget_is_an_error_and_leaves_the_project_alone() {
    // The model never produces a parseable brief. The bounded repair loop gives
    // up, and the caller decides what to do — the project is not involved.
    let dir = tempfile::tempdir().unwrap();
    let before = snapshot(dir.path());

    let provider = FakeProvider::new()
        .text("not a brief")
        .text("still not a brief")
        .text("nope");
    let error = draft_brief(&provider, "m", "greet the user", None)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("brief.md"), "{error}");
    assert_eq!(snapshot(dir.path()), before);
}

#[test]
fn approval_writes_exactly_the_reviewed_text() {
    let dir = tempfile::tempdir().unwrap();
    let draft = draft_of(BRIEF);

    persist_approved(dir.path(), &draft).unwrap();

    let written = std::fs::read_to_string(dir.path().join("brief.md")).unwrap();
    assert_eq!(
        written,
        draft.brief.render(),
        "what was approved is what is on disk"
    );
    // The provenance log is appended, not replaced, and carries the idea.
    let log = std::fs::read_to_string(dir.path().join(".localpilot").join("intake.jsonl")).unwrap();
    let record: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
    assert_eq!(record["idea"], "greet the user");
    assert_eq!(record["name"], "greeting");
    assert!(
        record.get("guidance").is_none(),
        "a gate-off run records no guidance object: {record}"
    );
}

#[test]
fn approving_twice_appends_to_the_log_rather_than_replacing_it() {
    let dir = tempfile::tempdir().unwrap();
    persist_approved(dir.path(), &draft_of(BRIEF)).unwrap();
    persist_approved(dir.path(), &draft_of(REVISED)).unwrap();

    let log = std::fs::read_to_string(dir.path().join(".localpilot").join("intake.jsonl")).unwrap();
    assert_eq!(log.lines().count(), 2, "the audit log is append-only");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("brief.md")).unwrap(),
        Brief::parse(REVISED).unwrap().render(),
        "the later approval is the one on disk"
    );
}

#[test]
fn a_failed_brief_write_changes_nothing_at_all() {
    // A directory where the brief belongs is the portable way to make the write
    // fail on every tier-1 OS. The assertion is unconditional on purpose: a test
    // that accepts either outcome keeps passing the day the injection stops
    // injecting.
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(project.join("brief.md")).unwrap();

    let error = persist_approved(&project, &draft_of(REVISED)).unwrap_err();

    assert!(
        matches!(error, Approval::BriefNotWritten(_)),
        "nothing was written, and the error says so: {error}"
    );
    assert!(
        project.join("brief.md").is_dir(),
        "the obstruction is untouched"
    );
    assert!(
        !project.join(".localpilot").join("intake.jsonl").exists(),
        "a failed brief write appends no audit record"
    );
}

#[test]
fn a_failed_record_append_still_reports_the_brief_as_saved() {
    // The two files fail differently and the caller has to tell them apart:
    // saying "brief.md was not written" after replacing it is worse than saying
    // nothing. A directory where the log belongs blocks the append.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".localpilot").join("intake.jsonl")).unwrap();

    let error = persist_approved(dir.path(), &draft_of(REVISED)).unwrap_err();

    assert!(
        matches!(error, Approval::RecordNotAppended(_)),
        "the brief is saved; only its provenance is missing: {error}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("brief.md")).unwrap(),
        Brief::parse(REVISED).unwrap().render(),
        "the brief really is on disk, so the error must not claim otherwise"
    );
}

#[test]
fn the_audit_record_keeps_the_users_idea_not_the_assembled_prompt() {
    // The log is a record of what a person asked for. Folding their answers into
    // a model prompt is an implementation detail of asking; it is not what they
    // said.
    let dir = tempfile::tempdir().unwrap();
    let mut draft = draft_of(BRIEF);
    draft.idea = "greet the user".to_string();
    draft.model_input =
        "greet the user\n\nDecisions provided by the user:\n- storage: none".to_string();

    persist_approved(dir.path(), &draft).unwrap();

    let log = std::fs::read_to_string(dir.path().join(".localpilot").join("intake.jsonl")).unwrap();
    let record: serde_json::Value = serde_json::from_str(log.trim()).unwrap();
    assert_eq!(record["idea"], "greet the user");
}

#[tokio::test]
async fn a_clarified_run_records_its_answers_even_when_all_were_delegated() {
    // The shipped log distinguishes "asked, and every answer was delegated" from
    // "never asked". Both end with the model deciding; only one of them asked.
    let provider = FakeProvider::new().text(SETTLED).text(BRIEF);
    let assessment = localpilot_harness::assess_guidance(&provider, "m", "an idea")
        .await
        .unwrap();

    let provider = FakeProvider::new().text(BRIEF);
    let clarified = localpilot_harness::draft_with_answers(
        &provider,
        "m",
        "an idea",
        0.7,
        assessment.clone(),
        vec!["Which platform?".to_string()],
        Vec::new(),
        true,
    )
    .await
    .unwrap();
    let json = clarified.guidance.to_json().unwrap();
    assert!(
        json.get("answers").is_some(),
        "the asked leg records: {json}"
    );
    assert_eq!(json["assumed_judgment"], true);

    let provider = FakeProvider::new().text(BRIEF);
    let assumed = localpilot_harness::draft_with_answers(
        &provider,
        "m",
        "an idea",
        0.7,
        assessment,
        vec!["Which platform?".to_string()],
        Vec::new(),
        false,
    )
    .await
    .unwrap();
    let json = assumed.guidance.to_json().unwrap();
    assert!(
        json.get("answers").is_none(),
        "the never-asked leg does not: {json}"
    );

    // Both legs are below the threshold, so both record the questions that were
    // put — the shipped log carried them on every below-threshold branch, and a
    // reader keys off them.
    for json in [
        clarified.guidance.to_json().unwrap(),
        assumed.guidance.to_json().unwrap(),
    ] {
        assert_eq!(
            json["questions"],
            serde_json::json!(["Which platform?"]),
            "the questions put are part of the record: {json}"
        );
    }
}

#[tokio::test]
async fn the_record_carries_the_questions_that_were_put_and_only_then() {
    // Regression against the shipped `intake_flow`: it set `guidance.questions`
    // before EVERY below-threshold branch and never above one. Losing it would
    // leave the audit trail unable to say what a user was actually asked.
    let provider = FakeProvider::new().text(SETTLED).text(BRIEF);
    let settled = localpilot_harness::draft_brief(
        &provider,
        "m",
        "a well-specified idea",
        Some(localpilot_harness::GuidanceParams {
            threshold: 0.7,
            max_questions: 5,
        }),
    )
    .await
    .unwrap();
    let localpilot_harness::DraftOutcome::Drafted(draft) = settled else {
        panic!("a settled idea drafts without asking");
    };
    let json = draft.guidance.to_json().unwrap();
    assert!(
        json.get("questions").is_none(),
        "nothing was asked above the threshold: {json}"
    );

    // Below it, the leg records what it asked — including when it asked nothing,
    // which is a different fact from never having got there.
    let provider = FakeProvider::new().text(SETTLED).text(BRIEF);
    let assessment = localpilot_harness::assess_guidance(&provider, "m", "an idea")
        .await
        .unwrap();
    let provider = FakeProvider::new().text(BRIEF);
    let none_put = localpilot_harness::draft_with_answers(
        &provider,
        "m",
        "an idea",
        0.7,
        assessment,
        Vec::new(),
        Vec::new(),
        true,
    )
    .await
    .unwrap();
    let json = none_put.guidance.to_json().unwrap();
    assert_eq!(
        json["questions"],
        serde_json::json!([]),
        "an empty list is still a record of the leg running: {json}"
    );
}
