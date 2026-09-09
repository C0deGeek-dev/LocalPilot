//! The harness workspace lifecycle: one read-only inspection, every state
//! separately observable, and nothing written by looking.
//!
//! Deliberately not in `lifecycle.rs` — that file is the *session* lifecycle
//! (resume, fork, clone, new). These are the brief/plan document states.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;

use localpilot_harness::{
    adopt_plan, inspect, resumable, AdoptError, Brief, BriefRevision, DocumentState,
    InterruptedRun, NotResumable, OperationLiveness, OperationState, Progress, WorkspaceInputs,
};

const BRIEF: &str = "# Brief: thing\n\n## Summary\n\nDo the thing.\n\n\
## Requirements\n\n- It works\n\n## Constraints\n\n- Be small\n\n\
## Non-Goals\n\n- World peace\n\n## Acceptance Criteria\n\n- A test passes\n";

/// A plan with no `Brief:` line: exactly what every project written before the
/// binding existed has on disk.
const LEGACY_PLAN: &str = "# Progress: thing\nBranch: feature/thing\n\n## Steps\n\n\
- [x] 1. Write a failing test\n  - commit: abc1234\n  - attempts: 2\n\
- [ ] 2. Implement it\n";

fn write(root: &Path, name: &str, contents: &str) {
    std::fs::write(root.join(name), contents).unwrap();
}

/// The plan bound to whatever `BRIEF` currently hashes to.
fn bound_plan() -> String {
    let revision = BriefRevision::of(&Brief::parse(BRIEF).unwrap());
    LEGACY_PLAN.replace(
        "Branch: feature/thing\n",
        &format!("Branch: feature/thing\nBrief: {revision}\n"),
    )
}

/// A plan carrying a binding written by some other build: the tag is not one
/// this build knows how to compare.
fn plan_bound_by_a_future_build() -> String {
    LEGACY_PLAN.replace(
        "Branch: feature/thing\n",
        "Branch: feature/thing\nBrief: sha256-v2:0000000000000000000000000000000000000000000000000000000000000000\n",
    )
}

fn state_at(root: &Path) -> DocumentState {
    inspect(WorkspaceInputs::at(root)).documents
}

#[test]
fn every_document_state_is_separately_observable() {
    // One table, one temp root per row: the point of the contract is that these
    // eleven situations produce eleven different answers, not one shared
    // "not ready".
    /// One row: a name, the files to lay down, and the state that must come back.
    type Case = (
        &'static str,
        Vec<(&'static str, String)>,
        fn(&DocumentState) -> bool,
    );

    let cases: Vec<Case> = vec![
        ("empty project", vec![], |s| {
            matches!(s, DocumentState::NoBrief)
        }),
        (
            "malformed brief",
            vec![("brief.md", "not a brief at all\n".to_string())],
            |s| matches!(s, DocumentState::BriefMalformed(_)),
        ),
        (
            "brief missing a required section",
            vec![(
                "brief.md",
                "# Brief: thing\n\n## Summary\n\nOnly this.\n".to_string(),
            )],
            |s| matches!(s, DocumentState::BriefMalformed(_)),
        ),
        ("brief only", vec![("brief.md", BRIEF.to_string())], |s| {
            matches!(s, DocumentState::BriefOnly { .. })
        }),
        (
            "malformed plan",
            vec![
                ("brief.md", BRIEF.to_string()),
                (
                    "PROGRESS.md",
                    "# Progress: thing\nno branch line\n".to_string(),
                ),
            ],
            |s| matches!(s, DocumentState::PlanMalformed { .. }),
        ),
        (
            "plan with a duplicate step number",
            vec![
                ("brief.md", BRIEF.to_string()),
                (
                    "PROGRESS.md",
                    LEGACY_PLAN.replace("- [ ] 2.", "- [ ] 1.").to_string(),
                ),
            ],
            |s| matches!(s, DocumentState::PlanMalformed { .. }),
        ),
        (
            "legacy plan with no binding",
            vec![
                ("brief.md", BRIEF.to_string()),
                ("PROGRESS.md", LEGACY_PLAN.to_string()),
            ],
            |s| matches!(s, DocumentState::PlanUnbound { .. }),
        ),
        (
            "plan bound to a different brief revision",
            vec![
                ("brief.md", BRIEF.to_string()),
                (
                    "PROGRESS.md",
                    LEGACY_PLAN.replace(
                        "Branch: feature/thing\n",
                        "Branch: feature/thing\nBrief: sha256-v1:1111111111111111111111111111111111111111111111111111111111111111\n",
                    ),
                ),
            ],
            |s| matches!(s, DocumentState::PlanStale { .. }),
        ),
        (
            "plan bound by a canonicalisation this build does not know",
            vec![
                ("brief.md", BRIEF.to_string()),
                ("PROGRESS.md", plan_bound_by_a_future_build()),
            ],
            |s| matches!(s, DocumentState::PlanBindingUnsupported { .. }),
        ),
        (
            "bound plan with work left",
            vec![
                ("brief.md", BRIEF.to_string()),
                ("PROGRESS.md", bound_plan()),
            ],
            |s| matches!(s, DocumentState::PlanReady { .. }),
        ),
        (
            "plan with two binding headers",
            vec![
                ("brief.md", BRIEF.to_string()),
                (
                    "PROGRESS.md",
                    bound_plan().replace(
                        "\n\n## Steps",
                        "\nBrief: sha256-v1:2222222222222222222222222222222222222222222222222222222222222222\n\n## Steps",
                    ),
                ),
            ],
            |s| matches!(s, DocumentState::PlanMalformed { .. }),
        ),
        (
            "brief that is not valid UTF-8",
            vec![],
            |s| matches!(s, DocumentState::BriefUnreadable(_)),
        ),
        (
            "bound plan with every step done",
            vec![
                ("brief.md", BRIEF.to_string()),
                ("PROGRESS.md", bound_plan().replace("- [ ] 2.", "- [x] 2.")),
            ],
            |s| matches!(s, DocumentState::PlanComplete { .. }),
        ),
    ];

    for (name, files, expected) in cases {
        let dir = tempfile::tempdir().unwrap();
        if name == "brief that is not valid UTF-8" {
            // A lone 0xFF byte is not valid UTF-8, so the read fails rather than
            // the parse: the file is present and unreadable, which is its own
            // state.
            std::fs::write(dir.path().join("brief.md"), [0xFF, 0xFE, 0xFF]).unwrap();
        }
        for (file, contents) in &files {
            write(dir.path(), file, contents);
        }
        let state = state_at(dir.path());
        assert!(expected(&state), "{name}: got {state:?}");
    }
}

#[test]
fn a_missing_plan_and_a_broken_plan_are_not_the_same_answer() {
    // The defect this contract exists to remove: both used to render as
    // "0/0 steps", so a corrupted plan looked like a project that had none.
    let missing = tempfile::tempdir().unwrap();
    write(missing.path(), "brief.md", BRIEF);

    let broken = tempfile::tempdir().unwrap();
    write(broken.path(), "brief.md", BRIEF);
    write(broken.path(), "PROGRESS.md", "# Progress: thing\n");

    assert!(matches!(
        state_at(missing.path()),
        DocumentState::BriefOnly { .. }
    ));
    let state = state_at(broken.path());
    match state {
        DocumentState::PlanMalformed { error, .. } => {
            assert!(
                error.to_string().contains("Branch:"),
                "the error names what is wrong: {error}"
            );
        }
        other => panic!("expected PlanMalformed, got {other:?}"),
    }
}

#[test]
fn a_missing_brief_and_a_broken_brief_are_not_the_same_answer() {
    let absent = tempfile::tempdir().unwrap();
    let malformed = tempfile::tempdir().unwrap();
    write(malformed.path(), "brief.md", "# Not A Brief\n");

    assert!(matches!(state_at(absent.path()), DocumentState::NoBrief));
    assert!(matches!(
        state_at(malformed.path()),
        DocumentState::BriefMalformed(_)
    ));
}

#[test]
fn editing_the_brief_makes_a_bound_plan_stale() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "brief.md", BRIEF);
    write(dir.path(), "PROGRESS.md", &bound_plan());
    assert!(matches!(
        state_at(dir.path()),
        DocumentState::PlanReady { .. }
    ));

    // A real requirement change, not a whitespace one.
    write(
        dir.path(),
        "brief.md",
        &BRIEF.replace("- It works", "- It works, and it is fast"),
    );
    assert!(
        matches!(state_at(dir.path()), DocumentState::PlanStale { .. }),
        "a changed requirement invalidates the plan built from it"
    );
}

#[test]
fn rewriting_the_brief_with_windows_line_endings_does_not_make_the_plan_stale() {
    // A Windows editor rewriting line endings is not a requirement change. If
    // it were, every cross-platform project would show a false stale plan the
    // user could not explain.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "brief.md", BRIEF);
    write(dir.path(), "PROGRESS.md", &bound_plan());
    assert!(matches!(
        state_at(dir.path()),
        DocumentState::PlanReady { .. }
    ));

    write(dir.path(), "brief.md", &BRIEF.replace('\n', "\r\n"));
    assert!(
        matches!(state_at(dir.path()), DocumentState::PlanReady { .. }),
        "CRLF is not a change to the requirements"
    );
}

#[test]
fn a_crlf_plan_parses_and_keeps_its_binding() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "brief.md", BRIEF);
    write(
        dir.path(),
        "PROGRESS.md",
        &bound_plan().replace('\n', "\r\n"),
    );
    assert!(matches!(
        state_at(dir.path()),
        DocumentState::PlanReady { .. }
    ));
}

#[test]
fn a_step_description_mentioning_brief_is_not_read_as_a_binding() {
    // The binding is a header field. A step that happens to talk about the
    // brief must not be mistaken for one, or a plan could bind itself by
    // accident.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "brief.md", BRIEF);
    write(
        dir.path(),
        "PROGRESS.md",
        &LEGACY_PLAN.replace(
            "- [ ] 2. Implement it",
            "- [ ] 2. Brief: rewrite the summary",
        ),
    );
    assert!(
        matches!(state_at(dir.path()), DocumentState::PlanUnbound { .. }),
        "only the header carries the binding"
    );
}

#[test]
fn inspection_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "brief.md", BRIEF);
    write(dir.path(), "PROGRESS.md", LEGACY_PLAN);

    let before = snapshot(dir.path());
    let _ = inspect(WorkspaceInputs {
        root: dir.path(),
        liveness: OperationLiveness::Running,
        interrupted: Some(InterruptedRun::QuotaPause),
    });
    let _ = state_at(dir.path());
    let after = snapshot(dir.path());

    assert_eq!(before, after, "inspection must not touch the project");
}

/// Every file in `root`, with its bytes and modification time.
fn snapshot(root: &Path) -> BTreeMap<String, (Vec<u8>, Option<SystemTime>)> {
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
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

#[test]
fn only_a_current_bound_plan_can_run() {
    let ready = tempfile::tempdir().unwrap();
    write(ready.path(), "brief.md", BRIEF);
    write(ready.path(), "PROGRESS.md", &bound_plan());
    let state = inspect(WorkspaceInputs::at(ready.path()));
    assert_eq!(
        resumable(&state).unwrap().next_incomplete().unwrap().number,
        2
    );

    let unbound = tempfile::tempdir().unwrap();
    write(unbound.path(), "brief.md", BRIEF);
    write(unbound.path(), "PROGRESS.md", LEGACY_PLAN);
    let state = inspect(WorkspaceInputs::at(unbound.path()));
    assert!(matches!(
        resumable(&state).unwrap_err(),
        NotResumable::Unbound
    ));

    let stale = tempfile::tempdir().unwrap();
    write(stale.path(), "brief.md", BRIEF);
    write(
        stale.path(),
        "PROGRESS.md",
        &bound_plan().replace("- It works", "- something else"),
    );
    write(
        stale.path(),
        "brief.md",
        &BRIEF.replace("- It works", "- It works twice"),
    );
    let state = inspect(WorkspaceInputs::at(stale.path()));
    assert!(matches!(
        resumable(&state).unwrap_err(),
        NotResumable::Stale { .. }
    ));

    let complete = tempfile::tempdir().unwrap();
    write(complete.path(), "brief.md", BRIEF);
    write(
        complete.path(),
        "PROGRESS.md",
        &bound_plan().replace("- [ ] 2.", "- [x] 2."),
    );
    let state = inspect(WorkspaceInputs::at(complete.path()));
    assert!(matches!(
        resumable(&state).unwrap_err(),
        NotResumable::Complete
    ));
}

#[test]
fn a_running_operation_blocks_a_second_one() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "brief.md", BRIEF);
    write(dir.path(), "PROGRESS.md", &bound_plan());
    let state = inspect(WorkspaceInputs {
        root: dir.path(),
        liveness: OperationLiveness::Running,
        interrupted: None,
    });
    assert_eq!(state.operation, OperationState::Active);
    assert!(matches!(
        resumable(&state).unwrap_err(),
        NotResumable::OperationActive
    ));
}

#[test]
fn a_recorded_pause_is_visible_but_does_not_block_resuming() {
    // The pause record is what the wait-and-resume path consumes, so it must
    // stay runnable — while still being reported, so a host can offer the right
    // command instead of a plain restart.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "brief.md", BRIEF);
    write(dir.path(), "PROGRESS.md", &bound_plan());
    let state = inspect(WorkspaceInputs {
        root: dir.path(),
        liveness: OperationLiveness::Idle,
        interrupted: Some(InterruptedRun::QuotaPause),
    });
    assert_eq!(
        state.operation,
        OperationState::Interrupted(InterruptedRun::QuotaPause)
    );
    assert!(resumable(&state).is_ok());
}

#[test]
fn adopting_binds_the_plan_and_preserves_every_piece_of_evidence() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "brief.md", BRIEF);
    write(dir.path(), "PROGRESS.md", LEGACY_PLAN);

    let before = Progress::parse(LEGACY_PLAN).unwrap();
    let revision = adopt_plan(dir.path()).unwrap();

    let after =
        Progress::parse(&std::fs::read_to_string(dir.path().join("PROGRESS.md")).unwrap()).unwrap();
    assert_eq!(after.brief_binding.as_deref(), Some(revision.as_str()));
    assert_eq!(
        after.steps, before.steps,
        "adoption writes the binding and nothing else: steps, commits, and attempts are untouched"
    );
    assert_eq!(after.name, before.name);
    assert_eq!(after.branch, before.branch);
    assert!(matches!(
        state_at(dir.path()),
        DocumentState::PlanReady { .. }
    ));

    // The brief itself is never rewritten by adoption.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("brief.md")).unwrap(),
        BRIEF
    );
}

#[test]
fn a_stale_plan_cannot_be_adopted_into_currency() {
    // Adoption answers "was this plan ever bound?", not "should this plan be
    // considered current?". A stale plan has a known-wrong binding, and
    // overwriting it would launder a superseded plan into a current one.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "brief.md", BRIEF);
    write(dir.path(), "PROGRESS.md", &bound_plan());
    write(
        dir.path(),
        "brief.md",
        &BRIEF.replace("- It works", "- It works differently"),
    );

    let error = adopt_plan(dir.path()).unwrap_err();
    assert!(
        matches!(&error, AdoptError::NotAdoptable(reason) if reason.contains("replan")),
        "got {error}"
    );
    assert!(matches!(
        state_at(dir.path()),
        DocumentState::PlanStale { .. }
    ));
}

#[test]
fn adopting_an_already_bound_plan_is_refused_rather_than_repeated() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "brief.md", BRIEF);
    write(dir.path(), "PROGRESS.md", &bound_plan());
    assert!(matches!(
        adopt_plan(dir.path()).unwrap_err(),
        AdoptError::NotAdoptable(_)
    ));
}

#[test]
fn the_binding_survives_a_restart() {
    // "Restart" for a file-backed contract means: read it again from scratch,
    // with nothing carried in memory.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "brief.md", BRIEF);
    write(dir.path(), "PROGRESS.md", LEGACY_PLAN);
    let revision = adopt_plan(dir.path()).unwrap();

    for _ in 0..3 {
        match state_at(dir.path()) {
            DocumentState::PlanReady { progress, .. } => {
                assert_eq!(progress.brief_binding.as_deref(), Some(revision.as_str()));
            }
            other => panic!("expected PlanReady after restart, got {other:?}"),
        }
    }
}

#[test]
fn a_bound_plan_round_trips_through_render() {
    let plan = bound_plan();
    let parsed = Progress::parse(&plan).unwrap();
    let reparsed = Progress::parse(&parsed.render()).unwrap();
    assert_eq!(parsed, reparsed);
    assert!(parsed.brief_binding.is_some());
}

#[test]
fn a_legacy_plan_round_trips_without_gaining_a_binding() {
    // Rendering an unbound plan must not invent a binding, or simply opening a
    // legacy project would silently declare it current.
    let parsed = Progress::parse(LEGACY_PLAN).unwrap();
    assert_eq!(parsed.brief_binding, None);
    assert_eq!(parsed.render(), LEGACY_PLAN);
}

#[test]
fn a_binding_from_another_build_is_unsupported_not_stale_and_does_not_run() {
    // Telling the user their plan is stale would assert that their requirements
    // changed. Nothing here supports that claim: this build simply cannot read
    // the tag. Saying so is the difference between a wrong answer and no answer.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "brief.md", BRIEF);
    write(dir.path(), "PROGRESS.md", &plan_bound_by_a_future_build());

    match state_at(dir.path()) {
        DocumentState::PlanBindingUnsupported { recorded, .. } => {
            assert!(recorded.starts_with("sha256-v2:"), "{recorded}");
        }
        other => panic!("expected PlanBindingUnsupported, got {other:?}"),
    }

    let state = inspect(WorkspaceInputs::at(dir.path()));
    assert!(matches!(
        resumable(&state).unwrap_err(),
        NotResumable::BindingUnsupported { .. }
    ));
}

#[test]
fn an_unreadable_binding_is_never_overwritten_by_adoption() {
    // Adoption would replace the recorded value — the only evidence of what
    // wrote it — with this build's own. That turns "I cannot read this" into "I
    // guarantee this", which is the same laundering a stale plan is refused for.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "brief.md", BRIEF);
    let original = plan_bound_by_a_future_build();
    write(dir.path(), "PROGRESS.md", &original);

    let error = adopt_plan(dir.path()).unwrap_err();
    assert!(
        matches!(&error, AdoptError::NotAdoptable(reason) if reason.contains("does not understand")),
        "got {error}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("PROGRESS.md")).unwrap(),
        original,
        "a refusal writes nothing"
    );
}

#[test]
fn the_recorded_binding_is_a_versioned_tag_and_a_full_digest() {
    // The value that reaches disk is the contract. A truncated digest labelled
    // with the full algorithm's name would be a lie in the one place a future
    // build has to trust.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "brief.md", BRIEF);
    write(dir.path(), "PROGRESS.md", LEGACY_PLAN);
    let revision = adopt_plan(dir.path()).unwrap();

    let (tag, digest) = revision.as_str().split_once(':').unwrap();
    assert_eq!(tag, "sha256-v1");
    assert_eq!(digest.len(), 64);
    assert!(std::fs::read_to_string(dir.path().join("PROGRESS.md"))
        .unwrap()
        .contains(&format!("Brief: {revision}")));
}

#[test]
fn a_document_that_cannot_be_read_names_the_path_and_the_cause() {
    // Present-but-unreadable is not missing, and the state carries the real
    // error rather than a generic "could not load". A directory where a file
    // belongs is the portable way to make a read fail on every tier-1 OS.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("brief.md")).unwrap();
    match state_at(dir.path()) {
        DocumentState::BriefUnreadable(error) => {
            let text = error.to_string();
            assert!(text.contains("brief.md"), "the path is named: {text}");
        }
        other => panic!("expected BriefUnreadable, got {other:?}"),
    }

    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "brief.md", BRIEF);
    std::fs::create_dir(dir.path().join("PROGRESS.md")).unwrap();
    match state_at(dir.path()) {
        DocumentState::PlanUnreadable { error, .. } => {
            let text = error.to_string();
            assert!(text.contains("PROGRESS.md"), "the path is named: {text}");
        }
        other => panic!("expected PlanUnreadable, got {other:?}"),
    }
}

#[test]
fn a_plan_that_is_not_valid_utf8_is_unreadable_not_malformed() {
    // The distinction matters for what the user is told to do: a malformed plan
    // is edited, an unreadable one is recovered.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "brief.md", BRIEF);
    std::fs::write(dir.path().join("PROGRESS.md"), [0x00, 0xFF, 0xFE]).unwrap();
    assert!(matches!(
        state_at(dir.path()),
        DocumentState::PlanUnreadable { .. }
    ));
}

#[test]
fn two_binding_headers_are_refused_rather_than_resolved_by_position() {
    // Silently taking the first would let a bad merge decide which requirements
    // a plan claims to satisfy.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "brief.md", BRIEF);
    write(
        dir.path(),
        "PROGRESS.md",
        &bound_plan().replace(
            "\n\n## Steps",
            "\nBrief: sha256-v1:3333333333333333333333333333333333333333333333333333333333333333\n\n## Steps",
        ),
    );
    match state_at(dir.path()) {
        DocumentState::PlanMalformed { error, .. } => {
            assert!(error.to_string().contains("Brief:"), "{error}");
        }
        other => panic!("expected PlanMalformed, got {other:?}"),
    }
}

#[test]
fn an_empty_binding_header_is_broken_rather_than_absent() {
    // A `Brief:` line with nothing after it is a binding that lost its value,
    // not a document from before bindings existed. Reading it as legacy-unbound
    // would make a damaged plan adoptable, quietly binding a plan whose recorded
    // revision is gone.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "brief.md", BRIEF);
    write(
        dir.path(),
        "PROGRESS.md",
        &LEGACY_PLAN.replace("Branch: feature/thing\n", "Branch: feature/thing\nBrief:\n"),
    );
    match state_at(dir.path()) {
        DocumentState::PlanMalformed { error, .. } => {
            assert!(error.to_string().contains("empty"), "{error}");
        }
        other => panic!("expected PlanMalformed, got {other:?}"),
    }
}

#[test]
fn two_binding_headers_are_refused_even_when_one_is_empty() {
    // Counting only non-empty values would let `Brief:` plus a real binding pass
    // as one binding, silently discarding evidence that the document disagrees
    // with itself.
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "brief.md", BRIEF);
    write(
        dir.path(),
        "PROGRESS.md",
        &bound_plan().replace("Branch: feature/thing\n", "Branch: feature/thing\nBrief:\n"),
    );
    match state_at(dir.path()) {
        DocumentState::PlanMalformed { error, .. } => {
            assert!(error.to_string().contains("2 'Brief:' lines"), "{error}");
        }
        other => panic!("expected PlanMalformed, got {other:?}"),
    }
}
