//! Eligibility and assignment building: only trusted sources, an oracle frozen
//! and independent of the fix, and an honest reason when no test exists.
//!
//! Every repository here is synthetic, built in a temporary directory, and so is
//! every security-flavoured example.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

use localmind_core::{
    AssignmentSource, CandidateLesson, CausalHypothesis, Confidence, EvidenceKind, EvidenceRef,
    HindsightDraft, LessonCategory, LessonId, Observation, OracleOrigin, SuggestedAction,
    VerdictReason,
};
use localpilot_config::CheckConfig;
use localpilot_harness::{check_command_digest, Progress};
use localpilot_localmind::{
    classify_for_lab, Eligibility, LabClassification, LabContext, RATIFIED_CHECK_KEY,
};

const SESSION: &str = "0b0e6c1e-0000-4000-8000-000000000001";

fn check(name: &str, program: &str, args: &[&str]) -> CheckConfig {
    toml::from_str(&format!(
        "name = {name:?}\nprogram = {program:?}\nargs = {args:?}\n"
    ))
    .unwrap()
}

fn test_check() -> CheckConfig {
    check("test", "cargo", &["test"])
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn commit(root: &Path, files: &[(&str, &str)], message: &str) -> String {
    for (path, content) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", message]);
    git(root, &["rev-parse", "HEAD"])
}

struct Repo {
    dir: tempfile::TempDir,
    base: String,
    fix: String,
}

/// A base with a test under `tests/`, and a step whose commit fixes `src/`.
/// `fix_files` lets a case make the fix touch the tests too.
fn repo(fix_files: &[(&str, &str)]) -> Repo {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "test@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    let base = commit(
        root,
        &[
            ("src/lib.rs", "pub fn users() -> bool { false }\n"),
            (
                "tests/users.rs",
                "#[test] fn users_exist() { assert!(app::users()); }\n",
            ),
        ],
        "base",
    );
    let fix = commit(root, fix_files, "harness: add the users table");
    Repo { dir, base, fix }
}

fn default_fix() -> Vec<(&'static str, &'static str)> {
    vec![("src/lib.rs", "pub fn users() -> bool { true }\n")]
}

fn progress(fix: &str) -> Progress {
    Progress::parse(&format!(
        "# Progress: users\nBranch: feature/users\n\n## Steps\n\n\
- [x] 1. Add the users table\n  - commit: {}\n  - attempts: 1\n  - sessions: {SESSION}\n",
        &fix[..7]
    ))
    .unwrap()
}

fn fact(label: &str, locator: &str) -> EvidenceRef {
    EvidenceRef::identified(
        EvidenceKind::ToolEvent,
        label,
        format!("localpilot-session:{SESSION}"),
        format!("localpilot-session:{SESSION}#event:{locator}"),
        format!("sha256:{locator}"),
    )
    .redacted()
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

fn check_ran(check: &CheckConfig, locator: &str, observation: Observation) -> EvidenceRef {
    let status = if observation == Observation::Failure {
        "failed"
    } else {
        "passed"
    };
    let mut fact = EvidenceRef::identified(
        EvidenceKind::TestOutput,
        format!("ratified check `{}` {status} (step)", check.name),
        format!("localpilot-session:{SESSION}"),
        format!("localpilot-session:{SESSION}#event:{locator}"),
        format!("sha256:{locator}"),
    )
    .redacted()
    .with_observation(observation)
    .with_signature(format!(
        "check:{}:{}",
        check.name,
        check_command_digest(check)
    ));
    fact.metadata
        .insert(RATIFIED_CHECK_KEY.to_string(), check.name.clone());
    fact
}

/// A candidate whose hindsight cites `cited` of `facts`.
fn candidate(lesson: &str, facts: Vec<EvidenceRef>, cited: &[usize]) -> CandidateLesson {
    candidate_with(
        lesson,
        "the users table did not exist yet",
        "write the migration before the test",
        "the first test run would have passed",
        facts,
        cited,
    )
}

/// [`candidate`] with its own cause and counterfactual, so a review chain
/// reads as one story.
fn candidate_with(
    lesson: &str,
    claim: &str,
    intervention: &str,
    counterfactual: &str,
    facts: Vec<EvidenceRef>,
    cited: &[usize],
) -> CandidateLesson {
    let mut draft = HindsightDraft::new("Store users in the database", "The tests pass");
    draft.proposed_lesson = Some(lesson.to_string());
    draft.intervention = Some(intervention.to_string());
    draft.counterfactual_prediction = Some(counterfactual.to_string());
    draft.preconditions = vec!["the database starts empty".to_string()];
    if !cited.is_empty() {
        draft = draft.with_hypothesis(CausalHypothesis {
            claim: claim.to_string(),
            evidence_ids: cited.iter().map(|index| facts[*index].id.clone()).collect(),
            confidence: Confidence::new(0.6).unwrap(),
        });
    }
    let mut candidate = CandidateLesson::new(
        LessonId::new("retro-1"),
        lesson,
        LessonCategory::Process,
        Confidence::new(0.4).unwrap(),
        SuggestedAction::PromoteToMemory,
    );
    for fact in facts {
        candidate = candidate.with_evidence(fact);
    }
    let candidate = candidate.with_hindsight(draft);
    assert!(candidate.validate_hindsight().is_ok());
    candidate
}

fn classify(
    candidate: &CandidateLesson,
    root: &Path,
    progress: Option<&Progress>,
    checks: &[CheckConfig],
) -> LabClassification {
    classify_for_lab(
        candidate,
        &LabContext {
            root,
            progress,
            checks,
        },
    )
}

/// A recorded fail → change → pass trajectory of a test command.
fn trajectory() -> Vec<EvidenceRef> {
    vec![
        failed("`run_shell` call `c1` failed", "c1", "run_shell:tests"),
        passed(
            "`write_file` call `c2` succeeded",
            "c2",
            "write_file:migration",
        ),
        passed("`run_shell` call `c3` succeeded", "c3", "run_shell:tests"),
    ]
}

const LESSON: &str = "Write the schema migration before the test that reads its table";

#[test]
fn a_recorded_fail_change_pass_trajectory_is_a_logic_assignment_with_every_part_recorded() {
    let facts = trajectory();
    let lesson = candidate(LESSON, facts.clone(), &[0, 1]);
    let empty = tempfile::tempdir().unwrap();

    let result = classify(&lesson, empty.path(), None, &[]);

    assert_eq!(result.eligibility, Eligibility::Logic, "{result:?}");
    let assignment = &result.assignments[0];
    assert_eq!(
        assignment.source,
        Some(AssignmentSource::RecordedTrajectory {
            session: format!("localpilot-session:{SESSION}")
        })
    );
    assert_eq!(assignment.candidate_identity, lesson.content_identity());
    assert_eq!(assignment.oracle.origin, OracleOrigin::Preexisting);
    assert_eq!(assignment.oracle.locator, facts[2].uri.clone().unwrap());
    assert!(
        !assignment.oracle.content_hash.is_empty() && !assignment.fixture.content_hash.is_empty()
    );
    assert_eq!(assignment.preconditions, vec!["the database starts empty"]);
    assert_eq!(
        assignment.counterfactual.as_deref(),
        Some("the first test run would have passed")
    );
    assert_eq!(assignment.allowed_tools, vec!["run_shell", "write_file"]);
    assert_eq!(
        assignment.success_observations,
        vec![facts[2].label.clone()]
    );
    assert_eq!(
        assignment.failure_observations,
        vec![facts[0].label.clone()]
    );
    assert_eq!(assignment.task_evidence.len(), 3);
    assert!(assignment.validate().is_ok());
}

#[test]
fn an_identical_retry_with_nothing_changed_is_not_a_test_of_anything() {
    let facts = vec![
        failed("`run_shell` call `c1` failed", "c1", "run_shell:tests"),
        passed("`run_shell` call `c2` succeeded", "c2", "run_shell:tests"),
    ];
    let lesson = candidate(LESSON, facts, &[0]);
    let empty = tempfile::tempdir().unwrap();

    let result = classify(&lesson, empty.path(), None, &[]);

    assert_eq!(result.eligibility, Eligibility::UpliftOnly);
    assert!(result.assignments.is_empty());
    assert!(result.reasons.contains(&VerdictReason::NoTrustedSource));
}

#[test]
fn a_ratified_check_that_failed_in_the_step_is_a_replay_fail_fix_pair() {
    let repo = repo(&default_fix());
    let root = repo.dir.path();
    let check = test_check();
    let facts = vec![
        check_ran(&check, "k1", Observation::Failure),
        passed(
            "`write_file` call `c2` succeeded",
            "c2",
            "write_file:migration",
        ),
    ];
    let lesson = candidate(LESSON, facts, &[0, 1]);
    let progress = progress(&repo.fix);

    let result = classify(&lesson, root, Some(&progress), &[check.clone()]);

    assert_eq!(result.eligibility, Eligibility::Replay, "{result:?}");
    assert_eq!(
        result.assignments.len(),
        1,
        "the step is HEAD, so no mutation is needed"
    );
    let assignment = &result.assignments[0];
    assert_eq!(
        assignment.source,
        Some(AssignmentSource::FailFixPair {
            base_revision: repo.base.clone(),
            fix_revision: repo.fix.clone()
        })
    );
    assert_eq!(
        assignment.oracle.locator,
        format!("ratified-check:test@{}", repo.base)
    );
    assert_eq!(assignment.oracle.origin, OracleOrigin::Preexisting);
    assert_eq!(assignment.fixture.locator, format!("git:{}", repo.base));
    assert_eq!(assignment.verifier.version, check_command_digest(&check));
    assert!(assignment.validate().is_ok());
}

#[test]
fn a_fix_the_code_has_moved_on_from_also_yields_a_controlled_mutation() {
    let repo = repo(&default_fix());
    let root = repo.dir.path();
    let head = commit(root, &[("README.md", "users\n")], "later work");
    let check = test_check();
    let facts = vec![check_ran(&check, "k1", Observation::Failure)];
    let lesson = candidate(LESSON, facts, &[0]);
    let progress = progress(&repo.fix);

    let result = classify(&lesson, root, Some(&progress), &[check]);

    let mutation = result
        .assignments
        .iter()
        .find(|assignment| {
            matches!(
                assignment.source,
                Some(AssignmentSource::ControlledMutation { .. })
            )
        })
        .expect("a mutation assignment");
    assert_eq!(
        mutation.source,
        Some(AssignmentSource::ControlledMutation {
            applied_to: head.clone(),
            repair_revision: repo.fix.clone()
        })
    );
    assert_eq!(
        mutation.fixture.locator,
        format!("git:{head}~revert:{}", repo.fix)
    );
}

#[test]
fn a_fix_that_also_changed_the_tests_cannot_be_judged_by_them() {
    for fix_files in [
        vec![
            ("src/lib.rs", "pub fn users() -> bool { true }\n"),
            ("tests/users.rs", "#[test] fn users_exist() {}\n"),
        ],
        vec![(
            "src/lib.rs",
            "pub fn users() -> bool { true }\n#[cfg(test)]\nmod tests { #[test] fn ok() {} }\n",
        )],
    ] {
        let repo = repo(&fix_files);
        let check = test_check();
        let facts = vec![check_ran(&check, "k1", Observation::Failure)];
        let lesson = candidate(LESSON, facts, &[0]);
        let progress = progress(&repo.fix);

        let result = classify(&lesson, repo.dir.path(), Some(&progress), &[check]);

        assert!(result.assignments.is_empty(), "{result:?}");
        assert_eq!(result.rejected[0].reason, VerdictReason::OracleChangedByFix);
        assert_eq!(result.eligibility, Eligibility::UpliftOnly);
        assert!(result.reasons.contains(&VerdictReason::OracleChangedByFix));
    }
}

#[test]
fn a_check_no_longer_ratified_as_it_ran_is_not_trusted() {
    let repo = repo(&default_fix());
    let ran = test_check();
    let facts = vec![check_ran(&ran, "k1", Observation::Failure)];
    let lesson = candidate(LESSON, facts, &[0]);
    let progress = progress(&repo.fix);
    let changed = check("test", "cargo", &["test", "--release"]);

    let result = classify(&lesson, repo.dir.path(), Some(&progress), &[changed]);

    assert!(result.assignments.is_empty());
    assert_eq!(result.rejected[0].reason, VerdictReason::NoTrustedSource);
}

#[test]
fn an_oracle_that_restates_the_lesson_is_refused() {
    let lesson_text = "Run the database migration before starting the application server";
    let facts = vec![
        failed("`run_shell` call `c1` failed", "c1", "run_shell:start"),
        passed(
            "`write_file` call `c2` succeeded",
            "c2",
            "write_file:config",
        ),
        passed(
            "run the database migration before starting the application server: ok",
            "c3",
            "run_shell:start",
        ),
    ];
    let lesson = candidate(lesson_text, facts, &[0, 1]);
    let empty = tempfile::tempdir().unwrap();

    let result = classify(&lesson, empty.path(), None, &[]);

    assert!(result.assignments.is_empty());
    assert_eq!(
        result.rejected[0].reason,
        VerdictReason::OracleNotIndependent
    );
}

#[test]
fn preferences_intent_unsafe_actions_and_unverifiable_style_are_honestly_not_executable() {
    let empty = tempfile::tempdir().unwrap();
    for (lesson, reason) in [
        (
            "Prefer small commits when working in this repository",
            VerdictReason::Preference,
        ),
        (
            "Confirm with the user which database the report should read",
            VerdictReason::HumanIntent,
        ),
        // Synthetic: nothing here names a real system or secret.
        (
            "Rotate the deploy credential before publishing a release",
            VerdictReason::UnsafeAction,
        ),
        (
            "Indent with tabs in this repository",
            VerdictReason::UnverifiableStyle,
        ),
    ] {
        // Even with a perfectly good trajectory, none of these is coerced into
        // a test.
        let lesson = candidate(lesson, trajectory(), &[0, 1]);

        let result = classify(&lesson, empty.path(), None, &[test_check()]);

        assert_eq!(result.eligibility, Eligibility::NotExecutable, "{result:?}");
        assert_eq!(result.reasons, vec![reason]);
        assert!(result.assignments.is_empty());
    }
}

#[test]
fn style_is_tested_only_by_the_check_that_verifies_style() {
    let empty = tempfile::tempdir().unwrap();
    let fmt = check("fmt", "cargo", &["fmt", "--check"]);

    // A test command's trajectory says nothing about indentation.
    let unrelated = candidate("Indent with tabs in this repository", trajectory(), &[0, 1]);
    let result = classify(&unrelated, empty.path(), None, &[fmt.clone()]);
    assert_eq!(result.eligibility, Eligibility::UpliftOnly, "{result:?}");
    assert!(result
        .reasons
        .contains(&VerdictReason::NoDiscriminatingVerifier));

    // The fmt check itself failing, a change, and it passing: that tests style.
    let facts = vec![
        check_ran(&fmt, "k1", Observation::Failure),
        passed("`edit_file` call `c2` succeeded", "c2", "edit_file:parser"),
        check_ran(&fmt, "k2", Observation::Success),
    ];
    let judged = candidate("Indent with tabs in this repository", facts, &[0, 1]);
    let result = classify(&judged, empty.path(), None, &[fmt]);
    assert_eq!(result.eligibility, Eligibility::Logic, "{result:?}");
}

#[test]
fn the_frozen_oracle_is_bound_to_the_base_revision_not_to_later_edits() {
    let repo = repo(&default_fix());
    let root = repo.dir.path();
    let check = test_check();
    let facts = vec![check_ran(&check, "k1", Observation::Failure)];
    let lesson = candidate(LESSON, facts, &[0]);
    let progress = progress(&repo.fix);

    let first = classify(&lesson, root, Some(&progress), &[check.clone()]);
    let again = classify(&lesson, root, Some(&progress), &[check.clone()]);
    assert_eq!(first, again, "classification is deterministic");

    // Someone — the treatment arm, say — rewrites the test afterwards.
    commit(
        root,
        &[("tests/users.rs", "#[test] fn users_exist() {}\n")],
        "weaken the test",
    );
    let later = classify(&lesson, root, Some(&progress), &[check]);

    let pair = |result: &LabClassification| {
        result
            .assignments
            .iter()
            .find(|assignment| {
                matches!(
                    assignment.source,
                    Some(AssignmentSource::FailFixPair { .. })
                )
            })
            .unwrap()
            .clone()
    };
    assert_eq!(
        pair(&first).identity(),
        pair(&later).identity(),
        "the pair's oracle is the base revision's test, unchanged by later edits"
    );
    let mutation = later
        .assignments
        .iter()
        .find(|assignment| {
            matches!(
                assignment.source,
                Some(AssignmentSource::ControlledMutation { .. })
            )
        })
        .unwrap();
    assert_ne!(
        mutation.oracle.content_hash,
        pair(&first).oracle.content_hash,
        "an oracle read at a later revision is a different oracle, and says so"
    );
}

/// The review corpus: each case's chain from facts through hindsight to the
/// assignment and its oracle, accepted and rejected alike. Printed, so a
/// reviewer reads what the code actually built. Run with `--nocapture`.
#[test]
fn review_corpus() {
    fn show(name: &str, lesson: &CandidateLesson, result: &LabClassification) {
        eprintln!("=== {name}");
        for fact in lesson.evidence() {
            eprintln!(
                "  fact {} [{}] {}{}",
                &fact.id.as_str()[..11],
                fact.kind.as_str(),
                fact.label,
                fact.signature()
                    .map(|s| format!("  sig={s}"))
                    .unwrap_or_default()
            );
        }
        if let Some(draft) = &lesson.hindsight {
            for hypothesis in &draft.hypotheses {
                eprintln!(
                    "  cause ({:.2}, cites {}): {}",
                    hypothesis.confidence.value(),
                    hypothesis
                        .evidence_ids
                        .iter()
                        .map(|id| &id.as_str()[..11])
                        .collect::<Vec<_>>()
                        .join(", "),
                    hypothesis.claim
                );
            }
            eprintln!("  counterfactual: {:?}", draft.counterfactual_prediction);
        }
        eprintln!("  lesson: {}", lesson.summary());
        eprintln!("  => {:?} {:?}", result.eligibility, result.reasons);
        for assignment in &result.assignments {
            eprintln!("  ACCEPTED {:?}", assignment.source);
            eprintln!(
                "    oracle {} origin={:?} hash={}",
                assignment.oracle.locator,
                assignment.oracle.origin,
                assignment
                    .oracle
                    .content_hash
                    .chars()
                    .take(19)
                    .collect::<String>()
            );
            eprintln!(
                "    fixture {} verifier {}@{}",
                assignment.fixture.locator, assignment.verifier.name, assignment.verifier.version
            );
            eprintln!("    task: {}", assignment.task);
            eprintln!(
                "    pass: {:?} / fail: {:?}",
                assignment.success_observations, assignment.failure_observations
            );
            eprintln!("    identity {}", assignment.identity());
        }
        for rejected in &result.rejected {
            eprintln!(
                "  REJECTED {:?}: {:?} — {}",
                rejected.source, rejected.reason, rejected.detail
            );
        }
    }

    let empty = tempfile::tempdir().unwrap();
    let lesson = candidate(LESSON, trajectory(), &[0, 1]);
    show(
        "recorded trajectory",
        &lesson,
        &classify(&lesson, empty.path(), None, &[]),
    );

    let repo = repo(&default_fix());
    commit(repo.dir.path(), &[("README.md", "users\n")], "later work");
    let check = test_check();
    let lesson = candidate(
        LESSON,
        vec![
            check_ran(&check, "k1", Observation::Failure),
            passed(
                "`write_file` call `c2` succeeded",
                "c2",
                "write_file:migration",
            ),
        ],
        &[0, 1],
    );
    let pair_progress = progress(&repo.fix);
    show(
        "ratified check fail/fix pair + mutation",
        &lesson,
        &classify(
            &lesson,
            repo.dir.path(),
            Some(&pair_progress),
            &[check.clone()],
        ),
    );

    let touched = repo_touching_tests();
    let lesson = candidate(
        LESSON,
        vec![check_ran(&check, "k1", Observation::Failure)],
        &[0],
    );
    let touched_progress = progress(&touched.fix);
    show(
        "fix also changed the test",
        &lesson,
        &classify(
            &lesson,
            touched.dir.path(),
            Some(&touched_progress),
            &[check.clone()],
        ),
    );

    let wording = "Run the database migration before starting the application server";
    let lesson = candidate(
        wording,
        vec![
            failed("`run_shell` call `c1` failed", "c1", "run_shell:start"),
            passed(
                "`write_file` call `c2` succeeded",
                "c2",
                "write_file:config",
            ),
            passed(
                "run the database migration before starting the application server: ok",
                "c3",
                "run_shell:start",
            ),
        ],
        &[0, 1],
    );
    show(
        "oracle restates the lesson",
        &lesson,
        &classify(&lesson, empty.path(), None, &[]),
    );

    let fmt = fmt_check();
    let lesson = candidate_with(
        "Indent with tabs in this repository",
        "the maintainer's formatter check rejected space indentation",
        "indent with tabs",
        "the formatter check would have passed the first time",
        vec![
            check_ran(&fmt, "k1", Observation::Failure),
            passed("`edit_file` call `c2` succeeded", "c2", "edit_file:parser"),
            check_ran(&fmt, "k2", Observation::Success),
        ],
        &[0, 1],
    );
    show(
        "style judged by its own ratified check",
        &lesson,
        &classify(&lesson, empty.path(), None, &[fmt.clone()]),
    );
    let lesson = candidate_with(
        "Indent with tabs in this repository",
        "the maintainer asked for tabs after the tests passed",
        "indent with tabs",
        "the edit would not have been redone",
        trajectory(),
        &[0, 1],
    );
    show(
        "style with only an unrelated trajectory",
        &lesson,
        &classify(&lesson, empty.path(), None, &[fmt]),
    );

    for (name, text, claim, intervention, counterfactual, facts) in [
        (
            "not executable: a preference",
            "Prefer small commits when working in this repository",
            "the reviewer asked for the change to be split",
            "split the change into smaller commits",
            "a smaller commit would not have been sent back",
            vec![fact(
                "driver `host` intervened: steer — please split this into smaller commits",
                "d1",
            )],
        ),
        (
            "not executable: an unsafe action (synthetic)",
            "Rotate the deploy credential before publishing a release",
            "the release job was refused with an expired credential",
            "rotate the credential first",
            "a fresh credential would have let the release job run",
            vec![failed(
                "`run_shell` call `c1` failed",
                "c1",
                "run_shell:release",
            )],
        ),
        (
            "not executable: style no ratified check verifies",
            "Indent with tabs in this repository",
            "the maintainer asked for tabs, and the edit used spaces",
            "indent with tabs",
            "an edit with tabs would not have been redone",
            vec![fact(
                "driver `host` intervened: steer — this repository indents with tabs",
                "d1",
            )],
        ),
    ] {
        let lesson = candidate_with(text, claim, intervention, counterfactual, facts, &[0]);
        show(
            name,
            &lesson,
            &classify(&lesson, empty.path(), None, &[test_check()]),
        );
    }
}

fn repo_touching_tests() -> Repo {
    repo(&[
        ("src/lib.rs", "pub fn users() -> bool { true }\n"),
        ("tests/users.rs", "#[test] fn users_exist() {}\n"),
    ])
}

fn fmt_check() -> CheckConfig {
    check("fmt", "cargo", &["fmt", "--check"])
}
