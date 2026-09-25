//! Eligibility over lessons a local model actually wrote.
//!
//! The markers that withhold a test are short English lists, and a model does
//! not phrase lessons the way a test author does. These are the distinct
//! lessons one local model proposed across two live hindsight runs, kept
//! verbatim, each classified with a valid recorded trajectory available — so a
//! `NotExecutable` here can only come from the markers.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use localmind_core::{
    CandidateLesson, CausalHypothesis, Confidence, EvidenceKind, EvidenceRef, HindsightDraft,
    LessonCategory, LessonId, Observation, SuggestedAction, VerdictReason,
};
use localpilot_config::CheckConfig;
use localpilot_localmind::{classify_for_lab, Eligibility, LabContext};

fn fact(label: &str, locator: &str, observation: Observation, signature: &str) -> EvidenceRef {
    EvidenceRef::identified(
        EvidenceKind::ToolEvent,
        label,
        "localpilot-session:live",
        format!("localpilot-session:live#event:{locator}"),
        format!("sha256:{locator}"),
    )
    .with_observation(observation)
    .with_signature(signature)
}

fn candidate(lesson: &str) -> CandidateLesson {
    let facts = [
        fact(
            "`run_shell` call `c1` failed",
            "c1",
            Observation::Failure,
            "run_shell:x",
        ),
        fact(
            "`write_file` call `c2` succeeded",
            "c2",
            Observation::Success,
            "write_file:y",
        ),
        fact(
            "`run_shell` call `c3` succeeded",
            "c3",
            Observation::Success,
            "run_shell:x",
        ),
    ];
    let draft = HindsightDraft::new("intended", "observed")
        .with_hypothesis(CausalHypothesis {
            claim: "the first attempt was missing a step".to_string(),
            evidence_ids: vec![facts[0].id.clone(), facts[1].id.clone()],
            confidence: Confidence::new(0.6).unwrap(),
        })
        .with_proposed_lesson(lesson);
    let mut candidate = CandidateLesson::new(
        LessonId::new("live"),
        lesson,
        LessonCategory::Process,
        Confidence::new(0.4).unwrap(),
        SuggestedAction::PromoteToMemory,
    );
    for fact in facts {
        candidate = candidate.with_evidence(fact);
    }
    candidate.with_hindsight(draft)
}

fn fmt_check() -> CheckConfig {
    toml::from_str("name = \"fmt\"\nprogram = \"cargo\"\nargs = [\"fmt\", \"--check\"]\n").unwrap()
}

/// Lessons about how code is indented or formatted.
const STYLE: &[&str] = &[
    "Always check existing code or configuration for indentation styles before making edits to ensure consistency.",
    "Before editing files in an unfamiliar repo, inspect existing indentation style (tabs vs spaces) to match conventions. (citing ev-31c72bc8ceafcda35e23b13b9b4526dc)",
    "Before generating code, verify and strictly adhere to the target repository's existing whitespace and indentation conventions (e.g., tabs vs. spaces) to avoid style-related corrections.",
    "Check and adhere to the repository's specific whitespace indentation style (e.g., tabs) before submitting edits.",
];

/// Everything else the model proposed. None of it is a preference, anyone's
/// intent, or a real-world action.
const TESTABLE: &[&str] = &[
    "Always verify acceptance criteria against fresh, logged tool outputs before marking a task as complete and committing changes.",
    "Always verify build success before marking a step complete and committing changes.",
    "Always verify that external destination drives are mounted before executing file export tasks.",
    "Always verify the database schema matches the application code before running integration tests.",
    "Configure shell execution tools to automatically retry network-dependent commands on timeout.",
    "Ensure database migrations are applied and verified before running integration tests that depend on schema structures.",
    "Never mark a build step as complete without verifying a successful exit code from the build command. Log failures should block completion.",
    "Verify that external drive mount points are available and accessible before attempting file exports.",
    "Verify that external drives are mounted before attempting file writes to avoid path-not-found errors and manual intervention.",
];

#[test]
fn real_lessons_are_withheld_only_where_the_markers_mean_it() {
    let root = tempfile::tempdir().unwrap();
    let classify = |lesson: &str, checks: &[CheckConfig]| {
        classify_for_lab(
            &candidate(lesson),
            &LabContext {
                root: root.path(),
                progress: None,
                checks,
            },
        )
    };

    for lesson in TESTABLE {
        let result = classify(lesson, &[]);
        eprintln!("{:?} {:?} — {lesson}", result.eligibility, result.reasons);
        assert_eq!(
            result.eligibility,
            Eligibility::Logic,
            "misfired on: {lesson}"
        );
    }
    for lesson in STYLE {
        let without = classify(lesson, &[]);
        eprintln!("{:?} {:?} — {lesson}", without.eligibility, without.reasons);
        assert_eq!(without.eligibility, Eligibility::NotExecutable, "{lesson}");
        assert_eq!(without.reasons, vec![VerdictReason::UnverifiableStyle]);

        // A ratified fmt check makes style verifiable, but this run's recorded
        // trajectory is a test command, which says nothing about indentation:
        // there is no oracle for the lesson here, so it waits for uplift.
        let with_fmt = classify(lesson, &[fmt_check()]);
        assert_eq!(with_fmt.eligibility, Eligibility::UpliftOnly, "{lesson}");
        assert!(with_fmt
            .reasons
            .contains(&VerdictReason::NoDiscriminatingVerifier));
    }
}
