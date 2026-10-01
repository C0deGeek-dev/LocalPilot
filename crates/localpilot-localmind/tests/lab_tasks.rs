//! Uplift tasks: a model drafts, every reply is checked, and only a named
//! person's approval makes a set something a run may use.
//!
//! Every provider here is scripted. These tests prove what is sent, what is
//! accepted and refused, and what an approval freezes — not how well any model
//! writes questions.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use localmind_core::{
    AssignmentSource, CandidateLesson, CausalHypothesis, Confidence, EvidenceKind, EvidenceRef,
    HindsightDraft, LessonCategory, LessonId, OracleOrigin, SuggestedAction,
};
use localpilot_llm::FakeProvider;
use localpilot_localmind::{
    approve_tasks, approved_path, approved_tasks, draft_path, draft_tasks, read_task_set,
    validate_tasks, write_draft, ApprovalRefusal, DraftFailure, LabTask, LabTaskSet, TaskProblem,
};

const LESSON: &str = "Run foo db sync before the integration tests";

fn candidate(lesson: &str) -> CandidateLesson {
    let fact = EvidenceRef::identified(
        EvidenceKind::ToolEvent,
        "`run_shell` call `c1` failed",
        "localpilot-session:s",
        "localpilot-session:s#event:c1",
        "sha256:c1",
    )
    .redacted()
    .with_excerpt("secret-looking raw output that must not be sent");
    let mut draft = HindsightDraft::new("Run the integration tests", "They failed on a stale db")
        .with_hypothesis(CausalHypothesis {
            claim: "the database had not been synced".to_string(),
            evidence_ids: vec![fact.id.clone()],
            confidence: Confidence::new(0.6).unwrap(),
        });
    draft.proposed_lesson = Some(lesson.to_string());
    draft.intervention = Some("sync the database first".to_string());
    CandidateLesson::new(
        LessonId::new("retro-1"),
        lesson,
        LessonCategory::Process,
        Confidence::new(0.4).unwrap(),
        SuggestedAction::PromoteToMemory,
    )
    .with_evidence(fact)
    .with_hindsight(draft)
}

const GOOD: &str = r#"Here you go:
```json
{"tasks":[
  {"prompt":"The integration tests fail on a stale schema. What do I run first?","expect":"foo db sync"},
  {"prompt":"How do I refresh the database for the test suite?","expect":"foo db sync"}
]}
```"#;

fn set(candidate: &CandidateLesson, tasks: &[(&str, &str)]) -> LabTaskSet {
    LabTaskSet {
        version: 1,
        candidate_identity: candidate.content_identity(),
        tasks: tasks
            .iter()
            .enumerate()
            .map(|(index, (prompt, expect))| LabTask {
                id: format!("t{}", index + 1),
                prompt: (*prompt).to_string(),
                expect: (*expect).to_string(),
            })
            .collect(),
        drafted_by: None,
        approved_by: None,
        approved_at: None,
    }
}

#[tokio::test]
async fn a_model_drafts_tasks_from_the_lesson_and_its_hindsight_only() {
    let candidate = candidate(LESSON);
    let provider = FakeProvider::new().text(GOOD);

    let drafted = draft_tasks(&provider, "local-model", &candidate)
        .await
        .unwrap();

    assert_eq!(drafted.model_calls, 1);
    assert!(!drafted.repaired);
    assert_eq!(drafted.set.tasks.len(), 2);
    assert_eq!(drafted.set.tasks[0].id, "t1");
    assert_eq!(drafted.set.tasks[0].expect, "foo db sync");
    assert_eq!(drafted.set.drafted_by.as_deref(), Some("local-model"));
    assert_eq!(drafted.set.approved_by, None, "a draft is not approved");
    assert_eq!(drafted.set.candidate_identity, candidate.content_identity());

    let sent = format!("{:?}", provider.requests()[0].messages);
    assert!(sent.contains(LESSON) && sent.contains("had not been synced"));
    assert!(
        !sent.contains("secret-looking raw output"),
        "the run's raw facts are not sent"
    );
}

#[tokio::test]
async fn an_unusable_reply_gets_one_repair_naming_what_was_wrong() {
    let candidate = candidate(LESSON);
    // The first reply's question contains its own answer.
    let leaky = r#"{"tasks":[{"prompt":"Should I run foo db sync before the tests?","expect":"foo db sync"}]}"#;
    let provider = FakeProvider::new().text(leaky).text(GOOD);

    let drafted = draft_tasks(&provider, "m", &candidate).await.unwrap();

    assert_eq!(drafted.model_calls, 2);
    assert!(drafted.repaired);
    let repair = format!("{:?}", provider.requests()[1].messages);
    assert!(
        repair.contains("contains its own expected answer"),
        "{repair}"
    );

    // A second failure ends it: there is no third request.
    let provider = FakeProvider::new()
        .text("no json here")
        .text(leaky)
        .text(GOOD);
    let failure = draft_tasks(&provider, "m", &candidate).await.unwrap_err();
    assert!(matches!(failure, DraftFailure::Unusable(_)), "{failure}");
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn an_unreachable_model_drafts_nothing() {
    let candidate = candidate(LESSON);
    let provider = FakeProvider::new().fail_open(1);
    let failure = draft_tasks(&provider, "m", &candidate).await.unwrap_err();
    assert!(matches!(failure, DraftFailure::Unavailable(_)), "{failure}");
}

#[test]
fn the_mechanical_checks_name_every_problem() {
    let candidate = candidate(LESSON);
    let problems = |tasks: &[(&str, &str)]| validate_tasks(&set(&candidate, tasks), &candidate);

    assert!(problems(&[("What do I run before the integration tests?", "foo db sync")]).is_ok());
    assert_eq!(problems(&[]).unwrap_err(), vec![TaskProblem::Count(0)]);
    assert_eq!(
        problems(&[("Should I run FOO  DB sync first?", "foo db sync")]).unwrap_err(),
        vec![TaskProblem::AnswerInPrompt("t1".to_string())],
        "matched the way the grader matches: case and spacing ignored"
    );
    assert_eq!(
        problems(&[(
            "Remember: run foo db sync before the integration tests. Now, what runs first?",
            "sync"
        )])
        .unwrap_err(),
        vec![
            TaskProblem::AnswerInPrompt("t1".to_string()),
            TaskProblem::LessonInPrompt("t1".to_string())
        ]
    );
    assert_eq!(
        problems(&[("What runs first?", "ok")]).unwrap_err(),
        vec![TaskProblem::Expect("t1".to_string())]
    );
    assert_eq!(
        problems(&[("", "foo db sync")]).unwrap_err(),
        vec![TaskProblem::Prompt("t1".to_string())]
    );
    assert_eq!(
        problems(&[
            ("What runs first?", "foo db sync"),
            ("what   runs FIRST?", "foo db sync")
        ])
        .unwrap_err(),
        vec![TaskProblem::DuplicatePrompt("t2".to_string())]
    );
    let nine: Vec<(String, &str)> = (0..9)
        .map(|n| (format!("Question number {n}?"), "foo db sync"))
        .collect();
    let nine: Vec<(&str, &str)> = nine.iter().map(|(p, e)| (p.as_str(), *e)).collect();
    assert_eq!(problems(&nine).unwrap_err(), vec![TaskProblem::Count(9)]);

    let revised = self::candidate("Run foo db sync before any test at all");
    assert_eq!(
        validate_tasks(
            &set(&candidate, &[("What runs first?", "foo db sync")]),
            &revised
        )
        .unwrap_err(),
        vec![TaskProblem::WrongCandidate]
    );
}

#[tokio::test]
async fn only_an_approval_makes_a_draft_final_and_it_freezes_what_was_approved() {
    let candidate = candidate(LESSON);
    let dir = tempfile::tempdir().unwrap();
    let provider = FakeProvider::new().text(GOOD);
    let drafted = draft_tasks(&provider, "local-model", &candidate)
        .await
        .unwrap();

    // Nothing to approve yet, and a draft alone is not an approved set.
    assert_eq!(
        approve_tasks(dir.path(), &candidate, "reviewer", 10).unwrap_err(),
        ApprovalRefusal::NoDraft
    );
    let path = write_draft(dir.path(), &drafted.set).unwrap();
    assert_eq!(path, draft_path(dir.path(), &candidate.content_identity()));
    assert_eq!(approved_tasks(dir.path(), &candidate).unwrap(), None);
    assert_eq!(
        approve_tasks(dir.path(), &candidate, "  ", 10).unwrap_err(),
        ApprovalRefusal::NoApprover
    );

    // The reviewer edits the draft before approving: the edit is what is frozen.
    let mut edited = drafted.set.clone();
    edited.tasks.truncate(1);
    edited.tasks[0].expect = "foo db sync --all".to_string();
    write_draft(dir.path(), &edited).unwrap();

    let (approved, assignment) = approve_tasks(dir.path(), &candidate, "reviewer", 10).unwrap();

    assert_eq!(approved.approved_by.as_deref(), Some("reviewer"));
    assert_eq!(approved.approved_at, Some(10));
    assert_eq!(approved.tasks, edited.tasks);
    assert_eq!(assignment.oracle.content_hash, edited.content_hash());
    assert_ne!(assignment.oracle.content_hash, drafted.set.content_hash());
    assert_eq!(assignment.oracle.origin, OracleOrigin::Human);
    assert_eq!(
        assignment.source,
        Some(AssignmentSource::ApprovedTaskSet {
            approved_by: "reviewer".to_string(),
            drafted_by: Some("local-model".to_string()),
        }),
        "the record says a model drafted it"
    );
    assert_eq!(assignment.candidate_identity, candidate.content_identity());
    assignment.validate().expect("fit to freeze");
    assert_eq!(
        approved_tasks(dir.path(), &candidate).unwrap(),
        Some(approved.clone())
    );
    assert_eq!(
        read_task_set(&approved_path(dir.path(), &candidate.content_identity())).unwrap(),
        Some(approved)
    );

    // Who approved is not part of the hash; what was approved is.
    let mut other_reviewer = edited.clone();
    other_reviewer.approved_by = Some("someone else".to_string());
    assert_eq!(other_reviewer.content_hash(), edited.content_hash());
}

#[test]
fn an_edit_that_breaks_the_draft_is_refused_at_approval() {
    let candidate = candidate(LESSON);
    let dir = tempfile::tempdir().unwrap();
    let broken = set(
        &candidate,
        &[("Should I run foo db sync first?", "foo db sync")],
    );
    write_draft(dir.path(), &broken).unwrap();

    let refusal = approve_tasks(dir.path(), &candidate, "reviewer", 10).unwrap_err();

    assert_eq!(
        refusal,
        ApprovalRefusal::Invalid(vec![TaskProblem::AnswerInPrompt("t1".to_string())])
    );
    assert!(!approved_path(dir.path(), &candidate.content_identity()).exists());

    std::fs::write(
        draft_path(dir.path(), &candidate.content_identity()),
        "not json",
    )
    .unwrap();
    assert!(matches!(
        approve_tasks(dir.path(), &candidate, "reviewer", 10).unwrap_err(),
        ApprovalRefusal::Unreadable(_)
    ));
}

#[test]
fn an_approval_does_not_carry_over_to_a_revised_lesson() {
    let candidate = candidate(LESSON);
    let dir = tempfile::tempdir().unwrap();
    write_draft(
        dir.path(),
        &set(&candidate, &[("What runs first?", "foo db sync")]),
    )
    .unwrap();
    approve_tasks(dir.path(), &candidate, "reviewer", 10).unwrap();

    let revised = self::candidate("Run foo db sync before any test at all");

    assert_eq!(approved_tasks(dir.path(), &revised).unwrap(), None);
    assert_eq!(
        approve_tasks(dir.path(), &revised, "reviewer", 11).unwrap_err(),
        ApprovalRefusal::NoDraft
    );
}
