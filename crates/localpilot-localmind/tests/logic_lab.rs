//! The Logic tier: a frozen recorded-trajectory assignment replayed against
//! virtual tools by the harness runtime, and judged from the run's own log.
//!
//! Every trajectory here is synthetic. These tests prove the mechanics — that a
//! sound assignment replays to `Valid`, that each way of being unsound is named,
//! that a run which cannot be read is never a finding about the lesson. They do
//! not show that any lesson helps: LocalMind D-LM-0048 reserves that for an
//! uplift run, and one test pins it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use localmind_core::{
    ArmRecord, AssignmentSource, CandidateLesson, CausalHypothesis, Confidence, EvidenceKind,
    EvidenceRef, EvidenceTier, ExperimentEvidence, ExperimentViolation, HindsightDraft, LabVerdict,
    LessonAssignment, LessonCategory, LessonId, Observation, OracleOrigin, SuggestedAction,
    VerdictReason,
};
use localpilot_localmind::{
    classify_for_lab, run_logic, Eligibility, LabContext, LogicOptions, INFRASTRUCTURE_FAILURE,
    LOGIC_RETRY_LIMIT, NOT_A_LOGIC_ASSIGNMENT, REPEAT_FLAGGED, STALE_ASSIGNMENT,
    WITHOUT_CHANGE_ARM, WITH_CHANGE_ARM,
};
use sha2::{Digest, Sha256};

const SESSION: &str = "0b0e6c1e-0000-4000-8000-000000000007";
const LESSON: &str = "Write the schema migration before the test that reads its table";

const FAILED: &str = "`run_shell` call `c1` failed";
const CHANGED: &str = "`write_file` call `c2` succeeded";
const PASSED: &str = "`run_shell` call `c3` succeeded";

fn fact_hashed(label: &str, locator: &str, hash: &str) -> EvidenceRef {
    EvidenceRef::identified(
        EvidenceKind::ToolEvent,
        label,
        format!("localpilot-session:{SESSION}"),
        format!("localpilot-session:{SESSION}#event:{locator}"),
        hash,
    )
    .redacted()
}

fn fact(label: &str, locator: &str) -> EvidenceRef {
    fact_hashed(label, locator, &format!("sha256:{locator}"))
}

fn failed(label: &str, locator: &str, signature: &str) -> EvidenceRef {
    fact(label, locator)
        .with_observation(Observation::Failure)
        .with_signature(signature)
}

fn passed(label: &str, locator: &str, signature: &str) -> EvidenceRef {
    fact(label, locator)
        .with_observation(Observation::Success)
        .with_signature(signature)
}

/// A test command failed, a migration was written, the same command passed.
fn trajectory() -> Vec<EvidenceRef> {
    vec![
        failed(FAILED, "c1", "run_shell:tests"),
        passed(CHANGED, "c2", "write_file:migration"),
        passed(PASSED, "c3", "run_shell:tests"),
    ]
}

fn candidate(lesson: &str, facts: &[EvidenceRef]) -> CandidateLesson {
    let mut draft = HindsightDraft::new("Store users in the database", "The tests pass")
        .with_hypothesis(CausalHypothesis {
            claim: "the users table did not exist yet".to_string(),
            evidence_ids: vec![facts[0].id.clone(), facts[1].id.clone()],
            confidence: Confidence::new(0.6).unwrap(),
        });
    draft.proposed_lesson = Some(lesson.to_string());
    draft.intervention = Some("write the migration before the test".to_string());
    let mut candidate = CandidateLesson::new(
        LessonId::new("retro-1"),
        lesson,
        LessonCategory::Process,
        Confidence::new(0.4).unwrap(),
        SuggestedAction::PromoteToMemory,
    );
    for fact in facts {
        candidate = candidate.with_evidence(fact.clone());
    }
    candidate.with_hindsight(draft)
}

/// A lesson, its frozen Logic assignment, and the record it was built from.
struct Fixture {
    candidate: CandidateLesson,
    assignment: LessonAssignment,
    recorded: Vec<EvidenceRef>,
}

fn fixture() -> Fixture {
    let recorded = trajectory();
    let candidate = candidate(LESSON, &recorded);
    let root = tempfile::tempdir().unwrap();
    let lab = classify_for_lab(
        &candidate,
        &LabContext {
            root: root.path(),
            progress: None,
            checks: &[],
        },
    );
    assert_eq!(lab.eligibility, Eligibility::Logic, "{lab:?}");
    let assignment = lab.assignments[0].clone();
    assert!(matches!(
        assignment.source,
        Some(AssignmentSource::RecordedTrajectory { .. })
    ));
    Fixture {
        candidate,
        assignment,
        recorded,
    }
}

fn options(scratch: &Path) -> LogicOptions {
    LogicOptions {
        scratch_root: Some(scratch.to_path_buf()),
        source_revision: "rev-1".to_string(),
        ..LogicOptions::default()
    }
}

async fn run(
    fixture: &Fixture,
    assignment: &LessonAssignment,
    recorded: &[EvidenceRef],
) -> ExperimentEvidence {
    let scratch = tempfile::tempdir().unwrap();
    run_logic(
        &fixture.candidate,
        assignment,
        recorded,
        &options(scratch.path()),
    )
    .await
}

/// The assignment with its trajectory replaced, and the fixture hash made to
/// match, as a hand-built assignment would be.
fn with_trajectory(assignment: &LessonAssignment, facts: &[&EvidenceRef]) -> LessonAssignment {
    let mut changed = assignment.clone();
    changed.task_evidence = facts.iter().map(|fact| fact.id.clone()).collect();
    let joined = changed
        .task_evidence
        .iter()
        .map(localmind_core::EvidenceId::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    let mut hash = String::from("sha256:");
    for byte in Sha256::digest(joined.as_bytes()) {
        hash.push_str(&format!("{byte:02x}"));
    }
    changed.fixture.content_hash = hash;
    changed
}

fn arm<'a>(evidence: &'a ExperimentEvidence, name: &str) -> &'a ArmRecord {
    evidence
        .arms
        .iter()
        .find(|arm| arm.arm == name)
        .unwrap_or_else(|| panic!("no {name} arm in {evidence:?}"))
}

fn is_empty_dir(path: &Path) -> bool {
    std::fs::read_dir(path).unwrap().next().is_none()
}

#[tokio::test]
async fn a_recorded_trajectory_replays_to_valid_through_the_harness_and_its_ledger() {
    let fixture = fixture();
    let scratch = tempfile::tempdir().unwrap();

    let evidence = run_logic(
        &fixture.candidate,
        &fixture.assignment,
        &fixture.recorded,
        &options(scratch.path()),
    )
    .await;

    assert_eq!(evidence.verdict, LabVerdict::Valid, "{evidence:?}");
    assert!(evidence.reasons.is_empty());
    assert_eq!(evidence.tier, EvidenceTier::Logic);
    evidence
        .validate(&fixture.candidate)
        .expect("a Valid Logic record satisfies the contract");
    assert_eq!(evidence.assignment.as_ref(), Some(&fixture.assignment));

    // Without the change: the recorded failure at every try, up to the limit,
    // and the harness's own breaker recognising the repeat.
    let without = arm(&evidence, WITHOUT_CHANGE_ARM);
    assert_eq!(without.attempts, LOGIC_RETRY_LIMIT);
    assert_eq!(without.passed, 0);
    assert_eq!(
        without.observations,
        vec![FAILED.to_string(), REPEAT_FLAGGED.to_string()]
    );
    // With the change: the failure, then — after the recorded change — the
    // recorded pass.
    let with = arm(&evidence, WITH_CHANGE_ARM);
    assert_eq!((with.attempts, with.passed), (2, 1));
    assert_eq!(
        with.observations,
        vec![FAILED.to_string(), PASSED.to_string()]
    );

    // Nothing but the world's own virtual tools took part, and the run's
    // temporary root is gone.
    let tools: Vec<(&str, &str)> = evidence
        .inputs
        .tool_versions
        .iter()
        .map(|(name, version)| (name.as_str(), version.as_str()))
        .collect();
    assert_eq!(
        tools,
        vec![("run_shell", "virtual/1"), ("write_file", "virtual/1")]
    );
    assert_eq!(evidence.inputs.model, None, "no model takes part");
    assert!(
        is_empty_dir(scratch.path()),
        "the temporary root is removed"
    );
    assert!(evidence.arms.iter().all(|arm| arm.logs.is_empty()));
    assert!(evidence.limitations[0].contains("says nothing about whether the lesson helps"));
}

#[tokio::test]
async fn two_runs_from_fresh_roots_agree_on_everything_but_timing() {
    let fixture = fixture();
    let first_root = tempfile::tempdir().unwrap();
    let second_root = tempfile::tempdir().unwrap();

    let first = run_logic(
        &fixture.candidate,
        &fixture.assignment,
        &fixture.recorded,
        &options(first_root.path()),
    )
    .await;
    let second = run_logic(
        &fixture.candidate,
        &fixture.assignment,
        &fixture.recorded,
        &options(second_root.path()),
    )
    .await;

    assert_eq!(first.verdict, second.verdict);
    assert_eq!(first.identity(), second.identity());
    assert_eq!(first.inputs, second.inputs);
    let untimed = |evidence: &ExperimentEvidence| {
        let mut evidence = evidence.clone();
        for arm in &mut evidence.arms {
            arm.wall_ms = 0;
        }
        evidence.provenance.produced_at = 0;
        evidence
    };
    assert_eq!(untimed(&first), untimed(&second));
    // Timing is reported, and kept out of what the result is bound to.
    let mut retimed = first.clone();
    retimed.arms[0].wall_ms += 1_000;
    assert_eq!(retimed.identity(), first.identity());
}

#[tokio::test]
async fn a_changed_oracle_makes_a_rerun_invalid() {
    let fixture = fixture();
    // The recorded pass is still at its place in the log, with other content.
    let mut recorded = fixture.recorded.clone();
    recorded[2] = fact_hashed(PASSED, "c3", "sha256:edited-afterwards")
        .with_observation(Observation::Success)
        .with_signature("run_shell:tests");

    let evidence = run(&fixture, &fixture.assignment, &recorded).await;

    assert_eq!(evidence.verdict, LabVerdict::Invalid);
    assert!(
        evidence.reasons.contains(&VerdictReason::OracleMutable),
        "{:?}",
        evidence.reasons
    );
    assert!(evidence.arms.is_empty(), "no arm runs on a changed oracle");
    evidence.validate(&fixture.candidate).unwrap();
}

#[tokio::test]
async fn a_recorded_pass_gone_from_the_record_is_a_missing_oracle() {
    let fixture = fixture();
    let recorded = fixture.recorded[..2].to_vec();

    let evidence = run(&fixture, &fixture.assignment, &recorded).await;

    assert_eq!(evidence.verdict, LabVerdict::Invalid);
    assert!(evidence.reasons.contains(&VerdictReason::OracleMutable));
}

#[tokio::test]
async fn an_oracle_with_no_frozen_content_is_mutable() {
    let fixture = fixture();
    let mut assignment = fixture.assignment.clone();
    assignment.oracle.content_hash = String::new();

    let evidence = run(&fixture, &assignment, &fixture.recorded).await;

    assert_eq!(evidence.verdict, LabVerdict::Invalid);
    assert_eq!(evidence.reasons, vec![VerdictReason::OracleMutable]);
}

#[tokio::test]
async fn a_recorded_change_gone_from_the_record_leaves_no_fixture() {
    let fixture = fixture();
    let recorded = vec![fixture.recorded[0].clone(), fixture.recorded[2].clone()];

    let evidence = run(&fixture, &fixture.assignment, &recorded).await;

    assert_eq!(evidence.verdict, LabVerdict::Invalid);
    assert_eq!(evidence.reasons, vec![VerdictReason::FixtureUnavailable]);
}

#[tokio::test]
async fn a_fixture_hash_that_is_not_the_trajectory_is_unavailable() {
    let fixture = fixture();
    let mut assignment = fixture.assignment.clone();
    assignment.fixture.content_hash = "sha256:some-other-trajectory".to_string();

    let evidence = run(&fixture, &assignment, &fixture.recorded).await;

    assert_eq!(evidence.verdict, LabVerdict::Invalid);
    assert_eq!(evidence.reasons, vec![VerdictReason::FixtureUnavailable]);
}

#[tokio::test]
async fn an_oracle_written_from_the_lesson_is_not_independent() {
    let fixture = fixture();
    let mut assignment = fixture.assignment.clone();
    assignment.oracle.origin = OracleOrigin::DerivedFromLesson;

    let evidence = run(&fixture, &assignment, &fixture.recorded).await;

    assert_eq!(evidence.verdict, LabVerdict::Invalid);
    assert_eq!(evidence.reasons, vec![VerdictReason::OracleNotIndependent]);
    evidence
        .validate(&fixture.candidate)
        .expect("Invalid is the verdict such an oracle must carry");
}

#[tokio::test]
async fn an_attempt_that_passes_without_any_change_discriminates_nothing() {
    let fixture = fixture();
    let facts = &fixture.recorded;
    let assignment = with_trajectory(&fixture.assignment, &[&facts[0], &facts[2]]);

    let evidence = run(&fixture, &assignment, &fixture.recorded).await;

    assert_eq!(evidence.verdict, LabVerdict::Invalid, "{evidence:?}");
    assert_eq!(
        evidence.reasons,
        vec![VerdictReason::NoDiscriminatingVerifier]
    );
    let without = arm(&evidence, WITHOUT_CHANGE_ARM);
    assert!(without.passed > 0, "both arms reach the pass");
}

#[tokio::test]
async fn a_required_observation_the_replay_never_shows_is_invalid() {
    let fixture = fixture();
    let mut assignment = fixture.assignment.clone();
    assignment.success_observations = vec!["all 3 user tests passed".to_string()];

    let evidence = run(&fixture, &assignment, &fixture.recorded).await;

    assert_eq!(evidence.verdict, LabVerdict::Invalid, "{evidence:?}");
    assert_eq!(evidence.reasons, vec![VerdictReason::NotReplayable]);
    let with = arm(&evidence, WITH_CHANGE_ARM);
    assert_eq!(with.passed, 1, "the pass was reached, just not as required");
}

#[tokio::test]
async fn a_trajectory_that_does_not_end_at_its_oracle_does_not_replay() {
    let fixture = fixture();
    let facts = &fixture.recorded;
    // Failure then the change, with the oracle still the recorded pass: the
    // trajectory stops short of what it claims to reach.
    let assignment = with_trajectory(&fixture.assignment, &[&facts[0], &facts[1]]);

    let evidence = run(&fixture, &assignment, &fixture.recorded).await;

    assert_eq!(evidence.verdict, LabVerdict::Invalid);
    assert_eq!(evidence.reasons, vec![VerdictReason::NotReplayable]);
}

#[tokio::test]
async fn a_cancelled_run_is_an_invalid_experiment_not_a_finding() {
    let fixture = fixture();
    let scratch = tempfile::tempdir().unwrap();
    let options = options(scratch.path());
    options.cancel.cancel();

    let evidence = run_logic(
        &fixture.candidate,
        &fixture.assignment,
        &fixture.recorded,
        &options,
    )
    .await;

    assert_eq!(evidence.verdict, LabVerdict::InvalidExperiment);
    assert_eq!(evidence.reasons, vec![VerdictReason::Cancelled]);
    assert!(arm(&evidence, WITHOUT_CHANGE_ARM).cancelled);
    evidence.validate(&fixture.candidate).unwrap();
}

#[tokio::test]
async fn a_breached_tool_budget_is_an_invalid_experiment() {
    let fixture = fixture();
    let scratch = tempfile::tempdir().unwrap();
    let options = LogicOptions {
        max_tool_calls: 1,
        ..options(scratch.path())
    };

    let evidence = run_logic(
        &fixture.candidate,
        &fixture.assignment,
        &fixture.recorded,
        &options,
    )
    .await;

    assert_eq!(
        evidence.verdict,
        LabVerdict::InvalidExperiment,
        "{evidence:?}"
    );
    assert_eq!(evidence.reasons, vec![VerdictReason::BudgetExceeded]);
    assert!(
        is_empty_dir(scratch.path()),
        "cleaned up after a stopped run"
    );
    evidence.validate(&fixture.candidate).unwrap();
}

#[tokio::test]
async fn a_root_that_cannot_be_made_is_an_infrastructure_failure() {
    let fixture = fixture();
    let dir = tempfile::tempdir().unwrap();
    let not_a_directory = dir.path().join("file");
    std::fs::write(&not_a_directory, "").unwrap();

    let evidence = run_logic(
        &fixture.candidate,
        &fixture.assignment,
        &fixture.recorded,
        &options(&not_a_directory),
    )
    .await;

    assert_eq!(evidence.verdict, LabVerdict::InvalidExperiment);
    assert_eq!(
        evidence.reasons,
        vec![VerdictReason::Other(INFRASTRUCTURE_FAILURE.to_string())]
    );
    evidence.validate(&fixture.candidate).unwrap();
}

#[tokio::test]
async fn a_changed_candidate_leaves_its_result_stale_and_its_assignment_unusable() {
    let fixture = fixture();
    let valid = run(&fixture, &fixture.assignment, &fixture.recorded).await;
    assert_eq!(valid.verdict, LabVerdict::Valid);

    let changed = candidate(
        "Write the schema migration first, and seed it",
        &fixture.recorded,
    );
    assert!(valid.is_stale_for(&changed));
    assert!(valid
        .validate(&changed)
        .unwrap_err()
        .contains(&ExperimentViolation::StaleCandidate));

    let scratch = tempfile::tempdir().unwrap();
    let rerun = run_logic(
        &changed,
        &fixture.assignment,
        &fixture.recorded,
        &options(scratch.path()),
    )
    .await;
    assert_eq!(rerun.verdict, LabVerdict::InvalidExperiment);
    assert_eq!(
        rerun.reasons,
        vec![VerdictReason::Other(STALE_ASSIGNMENT.to_string())]
    );
    assert_eq!(rerun.assignment, None);
    assert!(rerun.arms.is_empty());
    rerun.validate(&changed).unwrap();
}

#[tokio::test]
async fn only_recorded_trajectories_run_on_this_tier() {
    let fixture = fixture();
    let mut assignment = fixture.assignment.clone();
    assignment.source = Some(AssignmentSource::RatifiedCheck {
        name: "test".to_string(),
    });

    let evidence = run(&fixture, &assignment, &fixture.recorded).await;

    assert_eq!(evidence.verdict, LabVerdict::InvalidExperiment);
    assert_eq!(
        evidence.reasons,
        vec![VerdictReason::Other(NOT_A_LOGIC_ASSIGNMENT.to_string())]
    );
}

#[tokio::test]
async fn logic_can_never_claim_supported_contradicted_or_inconclusive() {
    let fixture = fixture();
    let valid = run(&fixture, &fixture.assignment, &fixture.recorded).await;

    for verdict in [
        LabVerdict::Supported,
        LabVerdict::Contradicted,
        LabVerdict::Inconclusive,
    ] {
        assert!(!verdict.permitted_for(EvidenceTier::Logic), "{verdict:?}");
        let mut claimed = valid.clone();
        claimed.verdict = verdict;
        let violations = claimed.validate(&fixture.candidate).unwrap_err();
        assert!(
            violations.contains(&ExperimentViolation::VerdictNotPermittedForTier {
                verdict,
                tier: EvidenceTier::Logic,
            }),
            "{verdict:?}: {violations:?}"
        );
    }
}

/// The complete verdict table: every verdict, which tier may emit it, and the
/// fixture behind each one Logic reaches. Run with `--nocapture` to print it.
#[tokio::test]
async fn verdict_table() {
    let fixture = fixture();
    let facts = &fixture.recorded;
    let mut rows: Vec<(String, String, String)> = Vec::new();
    let mut record = |evidence: &ExperimentEvidence, fixture_name: &str| {
        let reasons = evidence
            .reasons
            .iter()
            .map(|reason| format!("{reason:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        rows.push((
            format!("{:?}", evidence.verdict),
            reasons,
            fixture_name.to_string(),
        ));
    };

    record(
        &run(&fixture, &fixture.assignment, facts).await,
        "recorded fail → change → pass",
    );
    let mut altered = facts.clone();
    altered[2] = fact_hashed(PASSED, "c3", "sha256:edited-afterwards")
        .with_observation(Observation::Success)
        .with_signature("run_shell:tests");
    record(
        &run(&fixture, &fixture.assignment, &altered).await,
        "recorded pass edited after freezing",
    );
    record(
        &run(
            &fixture,
            &fixture.assignment,
            &[facts[0].clone(), facts[2].clone()],
        )
        .await,
        "a recorded change gone from the record",
    );
    let mut derived = fixture.assignment.clone();
    derived.oracle.origin = OracleOrigin::DerivedFromLesson;
    record(
        &run(&fixture, &derived, facts).await,
        "oracle written from the lesson",
    );
    record(
        &run(
            &fixture,
            &with_trajectory(&fixture.assignment, &[&facts[0], &facts[2]]),
            facts,
        )
        .await,
        "attempt passes with no change",
    );
    let mut unseen = fixture.assignment.clone();
    unseen.success_observations = vec!["all 3 user tests passed".to_string()];
    record(
        &run(&fixture, &unseen, facts).await,
        "required observation never shown",
    );
    let scratch = tempfile::tempdir().unwrap();
    let cancelled = options(scratch.path());
    cancelled.cancel.cancel();
    record(
        &run_logic(&fixture.candidate, &fixture.assignment, facts, &cancelled).await,
        "run cancelled",
    );
    let budget = LogicOptions {
        max_tool_calls: 1,
        ..options(scratch.path())
    };
    record(
        &run_logic(&fixture.candidate, &fixture.assignment, facts, &budget).await,
        "tool budget of 1",
    );
    let file = scratch.path().join("file");
    std::fs::write(&file, "").unwrap();
    record(
        &run_logic(
            &fixture.candidate,
            &fixture.assignment,
            facts,
            &options(&file),
        )
        .await,
        "temporary root cannot be made",
    );

    // NotExecutable comes from eligibility, before any assignment exists.
    let preference = candidate("Prefer plain SQL migrations in this repository", facts);
    let root = tempfile::tempdir().unwrap();
    let lab = classify_for_lab(
        &preference,
        &LabContext {
            root: root.path(),
            progress: None,
            checks: &[],
        },
    );
    assert_eq!(lab.eligibility, Eligibility::NotExecutable);
    rows.push((
        "NotExecutable".to_string(),
        format!("{:?}", lab.reasons[0]),
        "a preference (eligibility)".to_string(),
    ));

    let reached = |verdict: &str| rows.iter().any(|(v, _, _)| v == verdict);
    for verdict in ["Valid", "Invalid", "NotExecutable", "InvalidExperiment"] {
        assert!(reached(verdict), "Logic reaches {verdict}");
    }
    for verdict in ["Supported", "Contradicted", "Inconclusive"] {
        assert!(!reached(verdict), "Logic never reaches {verdict}");
    }

    println!("| Verdict | Logic | Replay | Uplift | Logic fixture | Reason |");
    println!("|---|---|---|---|---|---|");
    let all = [
        LabVerdict::Valid,
        LabVerdict::Invalid,
        LabVerdict::NotExecutable,
        LabVerdict::InvalidExperiment,
        LabVerdict::Supported,
        LabVerdict::Contradicted,
        LabVerdict::Inconclusive,
    ];
    let mark = |verdict: LabVerdict, tier: EvidenceTier| {
        if verdict.permitted_for(tier) {
            "yes"
        } else {
            "—"
        }
    };
    for verdict in all {
        let name = format!("{verdict:?}");
        let fixtures: Vec<&(String, String, String)> =
            rows.iter().filter(|(v, _, _)| *v == name).collect();
        let cells = |verdict| {
            (
                mark(verdict, EvidenceTier::Logic),
                mark(verdict, EvidenceTier::Replay),
                mark(verdict, EvidenceTier::Uplift),
            )
        };
        let (logic, replay, uplift) = cells(verdict);
        if fixtures.is_empty() {
            println!(
                "| {name} | {logic} | {replay} | {uplift} | unreachable on Logic (D-LM-0048) | — |"
            );
        }
        for (_, reasons, fixture_name) in fixtures {
            let reasons = if reasons.is_empty() { "—" } else { reasons };
            println!("| {name} | {logic} | {replay} | {uplift} | {fixture_name} | {reasons} |");
        }
    }
}
