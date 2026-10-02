//! Reviewer actions the lab has a stake in: rewrite, split, and asking for a
//! rerun.
//!
//! Every provider here is scripted and every store is a temporary directory.
//! These tests prove what each action writes and what it refuses — never how
//! well a model splits a lesson.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use localmind_core::{
    CandidateLesson, CausalHypothesis, Confidence, EvidenceKind, EvidenceRef, EvidenceTier,
    HindsightDraft, LessonCategory, LessonId, LessonRevision, ReviewState, SessionId,
    SuggestedAction,
};
use localmind_store::ReviewQueue;
use localpilot_llm::FakeProvider;
use localpilot_localmind::{
    approve_split, audit, clear_rerun, draft_split, lab_lesson_is_live, lab_lesson_state, promote,
    read_split_draft, request_rerun, rerun_requests, review_decide, review_rewrite, review_split,
    split_draft_path, splittable_item, validate_split, write_split_draft, DraftFailure,
    RerunRefusal, ReviewVerdict, SplitDraft, SplitProblem,
};

const LESSON: &str = "Run foo db sync and restart the workers before the integration tests";
const ITEM: &str = "retro-1";

fn candidate() -> CandidateLesson {
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
    draft.proposed_lesson = Some(LESSON.to_string());
    CandidateLesson::new(
        LessonId::new(ITEM),
        LESSON,
        LessonCategory::Process,
        Confidence::new(0.4).unwrap(),
        SuggestedAction::Split,
    )
    .with_evidence(fact)
    .with_hindsight(draft)
}

/// A project with one lesson pending in review.
fn project() -> (tempfile::TempDir, CandidateLesson) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".localmind.toml"),
        "[learning]\nenabled = true\nallowed_scopes = [\"project\"]\n",
    )
    .unwrap();
    let candidate = candidate();
    ReviewQueue::open_project(dir.path())
        .unwrap()
        .enqueue_candidates(
            &SessionId::new("completion-retrospective"),
            std::slice::from_ref(&candidate),
        )
        .unwrap();
    (dir, candidate)
}

fn localpilot_dir(root: &Path) -> std::path::PathBuf {
    root.join(".localpilot")
}

fn state(root: &Path, item: &str) -> ReviewState {
    ReviewQueue::open_project(root)
        .unwrap()
        .get(&localmind_core::ReviewItemId::new(item))
        .unwrap()
        .unwrap()
        .state
}

const PARTS: &str = r#"```json
{"parts":[
  "Run foo db sync before the integration tests",
  "Restart the workers before the integration tests"
]}
```"#;

#[test]
fn a_rewrite_through_the_adapter_audits_both_sides_and_promotes_the_new_text() {
    let (dir, original) = project();
    let root = dir.path();

    let rewritten = review_rewrite(
        root,
        ITEM,
        &LessonRevision {
            summary: Some("Run foo db sync before the integration tests".to_string()),
            cause: Some("the sync was skipped, the workers were fine".to_string()),
            ..LessonRevision::default()
        },
        "ada",
        Some("the workers were never the problem".to_string()),
    )
    .unwrap();

    assert_eq!(rewritten.original, ITEM);
    assert_eq!(rewritten.revised, "retro-1-r1");
    assert_eq!(state(root, ITEM), ReviewState::Merged);
    assert_eq!(state(root, "retro-1-r1"), ReviewState::Edited);
    // The lab finds the original by the identity its results are bound to, and
    // it reads as history; the rewrite has a different identity.
    let old = lab_lesson_state(root, &original.content_identity())
        .unwrap()
        .unwrap();
    assert!(!lab_lesson_is_live(&old));

    let subjects: Vec<String> = audit(root)
        .unwrap()
        .into_iter()
        .filter(|entry| entry.kind == "ReviewDecisionRecorded")
        .map(|entry| entry.subject)
        .collect();
    assert!(subjects.contains(&ITEM.to_string()));
    assert!(subjects.contains(&"retro-1-r1".to_string()));

    // Promoting from the id a caller still holds writes the rewrite.
    assert_eq!(promote(root, ITEM).unwrap(), "retro-1-r1");

    // Decided once.
    let again = review_rewrite(
        root,
        ITEM,
        &LessonRevision::of_summary("Something else entirely, said differently"),
        "ada",
        None,
    );
    assert!(again.unwrap_err().to_string().contains("it is history"));
}

#[test]
fn the_edit_verdict_every_surface_sends_is_the_same_rewrite() {
    let (dir, _) = project();
    let root = dir.path();

    // The terminal review's rewrite action and `learning review edit` both
    // arrive here as an `Edit` verdict.
    let shown = review_decide(
        root,
        ITEM,
        ReviewVerdict::Edit {
            replacement: "Run foo db sync before the integration tests".to_string(),
        },
        "tui",
        None,
    )
    .unwrap();

    assert_eq!(shown, "Edited");
    assert_eq!(state(root, ITEM), ReviewState::Merged);
    assert_eq!(state(root, "retro-1-r1"), ReviewState::Edited);
    let audited: Vec<String> = audit(root)
        .unwrap()
        .into_iter()
        .filter(|entry| entry.kind == "ReviewDecisionRecorded")
        .map(|entry| entry.subject)
        .collect();
    assert!(audited.contains(&ITEM.to_string()), "{audited:?}");
    assert!(audited.contains(&"retro-1-r1".to_string()), "{audited:?}");
}

#[tokio::test]
async fn a_model_drafts_a_split_and_only_an_approval_splits() {
    let (dir, _) = project();
    let root = dir.path();
    let store = localpilot_dir(root);
    let item = splittable_item(root, ITEM).unwrap();

    let provider = FakeProvider::new().text(PARTS);
    let drafted = draft_split(&provider, "local-model", &item).await.unwrap();
    assert_eq!(drafted.model_calls, 1);
    assert_eq!(drafted.draft.parts.len(), 2);
    assert_eq!(drafted.draft.drafted_by.as_deref(), Some("local-model"));
    let sent = format!("{:?}", provider.requests()[0].messages);
    assert!(sent.contains(LESSON) && sent.contains("had not been synced"));
    assert!(
        !sent.contains("secret-looking raw output"),
        "the run's raw facts are not sent"
    );

    // A draft changes nothing in review.
    let path = write_split_draft(&store, &drafted.draft).unwrap();
    assert_eq!(path, split_draft_path(&store, ITEM));
    assert_eq!(state(root, ITEM), ReviewState::Pending);
    assert_eq!(
        ReviewQueue::open_project(root)
            .unwrap()
            .list()
            .unwrap()
            .len(),
        1
    );

    // Nobody named, nothing split.
    assert!(approve_split(root, &store, ITEM, "  ", None)
        .unwrap_err()
        .to_string()
        .contains("named reviewer"));

    // The reviewer edits the file; what is approved is what is on disk.
    let mut edited = read_split_draft(&store, ITEM).unwrap().unwrap();
    edited.parts[1] = "Restart the workers after a schema sync".to_string();
    write_split_draft(&store, &edited).unwrap();

    let parts = approve_split(root, &store, ITEM, "ada", None).unwrap();

    assert_eq!(parts.len(), 2);
    assert_eq!(parts[1].summary, "Restart the workers after a schema sync");
    assert_eq!(state(root, ITEM), ReviewState::Merged);
    let queue = ReviewQueue::open_project(root).unwrap();
    let original = queue
        .get(&localmind_core::ReviewItemId::new(ITEM))
        .unwrap()
        .unwrap();
    assert_eq!(original.reviewer.as_deref(), Some("ada"));
    assert_eq!(
        original.note.as_deref(),
        Some("parts drafted by local-model, approved by ada")
    );
    for part in &parts {
        assert_eq!(state(root, &part.id), ReviewState::Pending);
    }
    assert!(
        read_split_draft(&store, ITEM).unwrap().is_none(),
        "an approved draft is consumed"
    );
    // The original is history now: no second split, no second draft.
    assert!(splittable_item(root, ITEM).is_err());
    assert!(review_split(
        root,
        ITEM,
        &[
            "One narrower lesson here".to_string(),
            "Another narrower lesson".to_string()
        ],
        "ada",
        None
    )
    .is_err());
}

#[tokio::test]
async fn an_unusable_split_reply_gets_one_repair_and_then_ends() {
    let (dir, _) = project();
    let item = splittable_item(dir.path(), ITEM).unwrap();

    let one_part = r#"{"parts":["Run foo db sync before the integration tests"]}"#;
    let provider = FakeProvider::new().text(one_part).text(PARTS);
    let drafted = draft_split(&provider, "m", &item).await.unwrap();
    assert!(drafted.repaired);
    assert_eq!(drafted.model_calls, 2);
    let repair = format!("{:?}", provider.requests()[1].messages);
    assert!(repair.contains("a split needs 2 to 5 parts"), "{repair}");

    let provider = FakeProvider::new().text(one_part).text("not json at all");
    assert!(matches!(
        draft_split(&provider, "m", &item).await,
        Err(DraftFailure::Unusable(_))
    ));
    let provider = FakeProvider::new().fail_open(1);
    assert!(matches!(
        draft_split(&provider, "m", &item).await,
        Err(DraftFailure::Unavailable(_))
    ));
}

#[test]
fn a_split_draft_is_checked_against_the_lesson_it_would_split() {
    let lesson = candidate();
    let draft = |parts: &[&str]| SplitDraft {
        version: 1,
        item_id: ITEM.to_string(),
        candidate_identity: lesson.content_identity(),
        parts: parts.iter().map(|part| (*part).to_string()).collect(),
        drafted_by: None,
    };
    let good = draft(&[
        "Run foo db sync before the integration tests",
        "Restart the workers before the integration tests",
    ]);
    assert_eq!(validate_split(&good, &lesson), Ok(()));

    let problems = |draft: &SplitDraft| validate_split(draft, &lesson).unwrap_err();
    assert_eq!(
        problems(&draft(&["Run foo db sync before the integration tests"])),
        vec![SplitProblem::PartCount(1)]
    );
    assert!(problems(&draft(&[
        "Run foo db sync before the integration tests",
        "run FOO db  sync before the integration tests.",
    ]))
    .contains(&SplitProblem::Duplicate(1, 2)));
    assert!(
        problems(&draft(&[LESSON, "Restart the workers before the tests"]))
            .contains(&SplitProblem::SameAsOriginal(1))
    );
    assert!(problems(&draft(&[
        "Too short",
        "Restart the workers before the tests"
    ]))
    .contains(&SplitProblem::PartLength(1)));

    // A draft written for a lesson that has since changed is not approved.
    let mut stale = good.clone();
    stale.candidate_identity = "cnd-somethingelse".to_string();
    assert_eq!(problems(&stale), vec![SplitProblem::StaleLesson]);
}

#[test]
fn a_rerun_request_is_a_note_and_runs_nothing() {
    let (dir, lesson) = project();
    let root = dir.path();
    let store = localpilot_dir(root);
    let identity = lesson.content_identity();

    assert_eq!(
        request_rerun(root, &store, &identity, EvidenceTier::Uplift, " ", None, 10),
        Err(RerunRefusal::NoReviewer)
    );
    assert_eq!(
        request_rerun(
            root,
            &store,
            &identity,
            EvidenceTier::Logic,
            "ada",
            None,
            10
        ),
        Err(RerunRefusal::Tier)
    );
    assert!(rerun_requests(&store, &identity).is_empty());

    let request = request_rerun(
        root,
        &store,
        &identity,
        EvidenceTier::Uplift,
        "ada",
        Some("the task set changed".to_string()),
        20,
    )
    .unwrap();
    request_rerun(
        root,
        &store,
        &identity,
        EvidenceTier::Replay,
        "bo",
        None,
        10,
    )
    .unwrap();

    assert_eq!(request.requested_by, "ada");
    let open = rerun_requests(&store, &identity);
    assert_eq!(open.len(), 2);
    assert_eq!(open[0].tier, EvidenceTier::Replay, "oldest first");
    assert_eq!(open[1].note.as_deref(), Some("the task set changed"));

    // Nothing ran and nothing was decided: no result, no state change, no run
    // directory, no memory.
    let queue = ReviewQueue::open_project(root).unwrap();
    let item = queue.list().unwrap().remove(0);
    assert_eq!(item.state, ReviewState::Pending);
    assert!(item.candidate.experiments.is_empty());
    assert!(!store.join("lab").join("uplift").exists());
    assert!(!store.join("lab").join("runs").exists());

    // Asking twice keeps one request per tier.
    request_rerun(
        root,
        &store,
        &identity,
        EvidenceTier::Uplift,
        "cy",
        None,
        30,
    )
    .unwrap();
    let open = rerun_requests(&store, &identity);
    assert_eq!(open.len(), 2);
    assert_eq!(open[1].requested_by, "cy");

    assert!(clear_rerun(&store, &identity, EvidenceTier::Uplift));
    assert!(!clear_rerun(&store, &identity, EvidenceTier::Uplift));
    assert_eq!(rerun_requests(&store, &identity).len(), 1);
}

#[test]
fn a_rerun_cannot_be_requested_for_a_lesson_that_is_history() {
    let (dir, lesson) = project();
    let root = dir.path();
    let store = localpilot_dir(root);
    let identity = lesson.content_identity();

    review_decide(root, ITEM, ReviewVerdict::Reject, "ada", None).unwrap();

    assert!(matches!(
        request_rerun(root, &store, &identity, EvidenceTier::Replay, "ada", None, 10),
        Err(RerunRefusal::NotLive(_, state)) if state == "Rejected"
    ));
    assert!(matches!(
        request_rerun(root, &store, "cnd-unknown", EvidenceTier::Replay, "ada", None, 10),
        Err(RerunRefusal::NotLive(_, state)) if state == "not in review"
    ));
    assert!(rerun_requests(&store, &identity).is_empty());

    // An accepted lesson is still live: a result can route it back to review.
    let (dir, lesson) = project();
    review_decide(dir.path(), ITEM, ReviewVerdict::Accept, "ada", None).unwrap();
    request_rerun(
        dir.path(),
        &localpilot_dir(dir.path()),
        &lesson.content_identity(),
        EvidenceTier::Uplift,
        "ada",
        None,
        10,
    )
    .unwrap();
}
