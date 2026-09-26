//! The Replay tier: a frozen fail/fix or mutation assignment, its ratified
//! check run on the commits it came from in temporary worktrees.
//!
//! Every repository is synthetic, built in a temporary directory, and every
//! check is a small command or script committed into it, so the same test runs
//! on Windows, Linux and macOS. The receipts are what review would read.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use localmind_core::{
    AssignmentSource, CandidateLesson, CausalHypothesis, Confidence, EvidenceKind, EvidenceRef,
    EvidenceTier, HindsightDraft, LabVerdict, LessonAssignment, LessonCategory, LessonId,
    Observation, SuggestedAction, VerdictReason,
};
use localpilot_config::{CheckConfig, Config};
use localpilot_harness::{check_command_digest, CancelSignal, Progress};
use localpilot_localmind::{
    classify_for_lab, plan_replay, replay_preview, run_replay, LabContext, ReplayOutcome,
    ReplayRefusal, CLEANUP_FAILED, EXPECT_FAIL_ARM, EXPECT_PASS_ARM, PERMISSION_DENIED,
    RATIFIED_CHECK_KEY,
};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile};

const SESSION: &str = "0b0e6c1e-0000-4000-8000-000000000008";
const LESSON: &str = "Write the state file before the check that reads it";

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

/// The ratified check: passes when `state.txt` holds exactly `fixed`.
#[cfg(windows)]
fn state_check() -> (&'static str, Vec<&'static str>) {
    ("findstr", vec!["/b", "/c:fixed", "state.txt"])
}
#[cfg(not(windows))]
fn state_check() -> (&'static str, Vec<&'static str>) {
    ("grep", vec!["-qx", "fixed", "state.txt"])
}

/// A check that runs a committed script named `name` (without extension).
#[cfg(windows)]
fn script_check(name: &str) -> (&'static str, Vec<String>) {
    ("cmd", vec!["/C".to_string(), format!("{name}.cmd")])
}
#[cfg(not(windows))]
fn script_check(name: &str) -> (&'static str, Vec<String>) {
    ("sh", vec![format!("{name}.sh")])
}

/// Committed scripts every project carries, for both families of OS. Each ends
/// by running the state check, so it still fails before the fix and passes
/// after it.
fn scripts() -> Vec<(&'static str, String)> {
    let big = "x".repeat(99) + "\n";
    vec![
        (
            "hang.cmd",
            "@echo off\r\nstart /b cmd /c beat.cmd\r\nping -n 60 127.0.0.1 >nul\r\n".to_string(),
        ),
        (
            "beat.cmd",
            "@echo off\r\n:loop\r\necho x>>..\\..\\heartbeat.txt\r\nping -n 2 127.0.0.1 >nul\r\ngoto loop\r\n"
                .to_string(),
        ),
        (
            "hang.sh",
            "(while true; do echo x >> ../../heartbeat.txt; sleep 0.2; done) &\nsleep 60\n"
                .to_string(),
        ),
        (
            "slow.cmd",
            "@echo off\r\nping -n 4 127.0.0.1 >nul\r\nfindstr /b /c:fixed state.txt\r\n".to_string(),
        ),
        ("slow.sh", "sleep 3\ngrep -qx fixed state.txt\n".to_string()),
        (
            "flood.cmd",
            "@echo off\r\ntype big.txt\r\nfindstr /b /c:fixed state.txt\r\n".to_string(),
        ),
        (
            "flood.sh",
            "cat big.txt\ngrep -qx fixed state.txt\n".to_string(),
        ),
        (
            "mutate.cmd",
            "@echo off\r\necho mutated>>tests\\users.txt\r\nfindstr /b /c:fixed state.txt\r\n"
                .to_string(),
        ),
        (
            "mutate.sh",
            "echo mutated >> tests/users.txt\ngrep -qx fixed state.txt\n".to_string(),
        ),
        ("big.txt", big.repeat(20_000)),
    ]
}

fn config_text(enabled: bool, program: &str, args: &[String]) -> String {
    let lab = if enabled {
        "[lab]\nreplay = true\n\n"
    } else {
        ""
    };
    format!("{lab}[[harness.checks]]\nname = \"test\"\nprogram = {program:?}\nargs = {args:?}\n")
}

/// A synthetic project: a base where the state is broken, a step commit that
/// fixes it, and (when `moved_on`) a later commit so a mutation exists.
struct Project {
    dir: tempfile::TempDir,
    fix: String,
    check: CheckConfig,
}

impl Project {
    fn root(&self) -> &Path {
        self.dir.path()
    }
}

fn project_with(program: &str, args: &[String], enabled: bool, moved_on: bool) -> Project {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "test@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "core.autocrlf", "false"]);
    let config = config_text(enabled, program, args);
    let mut files: Vec<(&str, String)> = scripts();
    files.push((".localpilot.toml", config.clone()));
    files.push((".gitignore", ".localpilot/\n".to_string()));
    files.push(("state.txt", "broken\n".to_string()));
    files.push(("tests/users.txt", "the users test\n".to_string()));
    let borrowed: Vec<(&str, &str)> = files.iter().map(|(p, c)| (*p, c.as_str())).collect();
    commit(root, &borrowed, "base");
    let fix = commit(
        root,
        &[("state.txt", "fixed\n")],
        "harness: write the state",
    );
    if moved_on {
        commit(root, &[("notes.md", "later work\n")], "later");
    }
    let parsed: Config = toml::from_str(&config).unwrap();
    let check = parsed.harness.checks[0].clone();
    Project { dir, fix, check }
}

fn project(moved_on: bool) -> Project {
    let (program, args) = state_check();
    let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
    project_with(program, &args, true, moved_on)
}

fn scripted(name: &str) -> Project {
    let (program, args) = script_check(name);
    project_with(program, &args, true, false)
}

fn progress(fix: &str) -> Progress {
    Progress::parse(&format!(
        "# Progress: state\nBranch: feature/state\n\n## Steps\n\n\
- [x] 1. Write the state\n  - commit: {}\n  - attempts: 1\n  - sessions: {SESSION}\n",
        &fix[..7]
    ))
    .unwrap()
}

/// A lesson whose hindsight cites the ratified check failing in the step.
fn candidate(check: &CheckConfig) -> CandidateLesson {
    let mut failed = EvidenceRef::identified(
        EvidenceKind::TestOutput,
        "ratified check `test` failed (step)",
        format!("localpilot-session:{SESSION}"),
        format!("localpilot-session:{SESSION}#event:c1"),
        "sha256:c1",
    )
    .redacted()
    .with_observation(Observation::Failure)
    .with_signature(format!("check:test:{}", check_command_digest(check)));
    failed
        .metadata
        .insert(RATIFIED_CHECK_KEY.to_string(), "test".to_string());
    let mut draft = HindsightDraft::new("Write the state", "The check passes").with_hypothesis(
        CausalHypothesis {
            claim: "the state file had not been written yet".to_string(),
            evidence_ids: vec![failed.id.clone()],
            confidence: Confidence::new(0.6).unwrap(),
        },
    );
    draft.proposed_lesson = Some(LESSON.to_string());
    draft.intervention = Some("write the state first".to_string());
    CandidateLesson::new(
        LessonId::new("retro-1"),
        LESSON,
        LessonCategory::Process,
        Confidence::new(0.4).unwrap(),
        SuggestedAction::PromoteToMemory,
    )
    .with_evidence(failed)
    .with_hindsight(draft)
}

/// The project's frozen Replay assignment of the given kind.
fn frozen(project: &Project, mutation: bool) -> (CandidateLesson, LessonAssignment) {
    let candidate = candidate(&project.check);
    let progress = progress(&project.fix);
    let lab = classify_for_lab(
        &candidate,
        &LabContext {
            root: project.root(),
            progress: Some(&progress),
            checks: std::slice::from_ref(&project.check),
        },
    );
    let assignment = lab
        .assignments
        .iter()
        .find(|assignment| {
            matches!(
                (&assignment.source, mutation),
                (Some(AssignmentSource::FailFixPair { .. }), false)
                    | (Some(AssignmentSource::ControlledMutation { .. }), true)
            )
        })
        .unwrap_or_else(|| panic!("no frozen assignment in {lab:?}"))
        .clone();
    (candidate, assignment)
}

fn bypass() -> PermissionEngine {
    PermissionEngine::new(Profile::Bypass, Vec::new())
}

async fn replay_with(
    project: &Project,
    mutation: bool,
    engine: &PermissionEngine,
    interactivity: Interactivity,
    timeout: Duration,
    cancel: &CancelSignal,
) -> (CandidateLesson, ReplayOutcome) {
    let (candidate, assignment) = frozen(project, mutation);
    let plan = plan_replay(project.root(), &candidate, &assignment, timeout).unwrap();
    let outcome = run_replay(project.root(), &plan, engine, interactivity, cancel).await;
    (candidate, outcome)
}

async fn replay(project: &Project, mutation: bool) -> (CandidateLesson, ReplayOutcome) {
    replay_with(
        project,
        mutation,
        &bypass(),
        Interactivity::NonInteractive,
        Duration::from_secs(120),
        &CancelSignal::new(),
    )
    .await
}

fn lab_worktrees(root: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(root.join(".localpilot").join("worktrees"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("lab-"))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn status(root: &Path) -> String {
    git(root, &["status", "--porcelain"])
}

#[tokio::test]
async fn a_fail_fix_pair_replays_to_valid_and_leaves_nothing_behind() {
    let project = project(false);
    let before = status(project.root());
    let (candidate, assignment) = frozen(&project, false);
    let plan = plan_replay(
        project.root(),
        &candidate,
        &assignment,
        Duration::from_secs(120),
    )
    .unwrap();

    let shown = replay_preview(&plan);
    assert!(
        shown.contains(&format!("command: {}", state_check().0)),
        "{shown}"
    );
    assert!(
        shown.contains("not a sandbox") && shown.contains("network"),
        "{shown}"
    );
    assert!(shown.contains("CARGO_TARGET_DIR="), "{shown}");
    assert!(
        lab_worktrees(project.root()).is_empty(),
        "a preview runs nothing"
    );

    let outcome = run_replay(
        project.root(),
        &plan,
        &bypass(),
        Interactivity::NonInteractive,
        &CancelSignal::new(),
    )
    .await;

    let evidence = &outcome.evidence;
    assert_eq!(
        evidence.verdict,
        LabVerdict::Valid,
        "{:#?}",
        outcome.receipt
    );
    assert_eq!(evidence.tier, EvidenceTier::Replay);
    evidence.validate(&candidate).unwrap();
    let arms = &outcome.receipt.arms;
    assert_eq!(arms.len(), 2);
    assert_eq!(arms[0].name, EXPECT_FAIL_ARM);
    assert!(
        !arms[0].passed && arms[0].exit_code == Some(1),
        "{:?}",
        arms[0]
    );
    assert_eq!(arms[1].name, EXPECT_PASS_ARM);
    assert!(arms[1].passed, "{:?}", arms[1]);
    assert!(arms.iter().all(|arm| arm.cleanup == "removed"));
    assert!(lab_worktrees(project.root()).is_empty());
    assert!(outcome.receipt.main_checkout_unchanged);
    assert_eq!(
        status(project.root()),
        before,
        "the main checkout is untouched"
    );
    let path = outcome.receipt_path.as_ref().unwrap();
    assert!(path.is_file());
    assert!(evidence.arms.iter().all(|arm| arm.logs.len() == 1));
    assert!(!outcome
        .receipt
        .env_names
        .iter()
        .any(|name| name == "GITHUB_TOKEN"));
}

#[tokio::test]
async fn a_controlled_mutation_replays_to_valid() {
    let project = project(true);
    let (candidate, outcome) = replay(&project, true).await;
    assert_eq!(
        outcome.evidence.verdict,
        LabVerdict::Valid,
        "{:#?}",
        outcome.receipt
    );
    outcome.evidence.validate(&candidate).unwrap();
    assert!(outcome.receipt.arms[0].revert.is_some());
    assert!(lab_worktrees(project.root()).is_empty());
}

#[tokio::test]
async fn replay_runs_only_where_the_committed_config_allows_it() {
    let (program, args) = state_check();
    let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
    let off = project_with(program, &args, false, false);
    let (candidate, assignment) = frozen(&off, false);
    assert_eq!(
        plan_replay(off.root(), &candidate, &assignment, Duration::from_secs(60)).unwrap_err(),
        ReplayRefusal::NotEnabled
    );

    // Enabled only in the working copy: not the trust boundary.
    std::fs::write(
        off.root().join(".localpilot.toml"),
        config_text(true, program, &args),
    )
    .unwrap();
    let refusal =
        plan_replay(off.root(), &candidate, &assignment, Duration::from_secs(60)).unwrap_err();
    assert!(
        matches!(refusal, ReplayRefusal::Untrusted(_)),
        "{refusal:?}"
    );
    assert!(lab_worktrees(off.root()).is_empty());
}

#[tokio::test]
async fn a_check_changed_after_freezing_is_a_changed_oracle() {
    let project = project(false);
    let (candidate, assignment) = frozen(&project, false);
    let (program, _) = state_check();
    let changed = config_text(true, program, &["/i".to_string(), "fixed".to_string()]);
    commit(
        project.root(),
        &[(".localpilot.toml", &changed)],
        "loosen the check",
    );

    let refusal = plan_replay(
        project.root(),
        &candidate,
        &assignment,
        Duration::from_secs(60),
    )
    .unwrap_err();
    let ReplayRefusal::Unsound { reasons, .. } = &refusal else {
        panic!("{refusal:?}");
    };
    assert_eq!(reasons, &vec![VerdictReason::OracleMutable]);
    let evidence = refusal.evidence(&candidate, &assignment, "rev").unwrap();
    assert_eq!(evidence.verdict, LabVerdict::Invalid);
    evidence.validate(&candidate).unwrap();
    assert!(lab_worktrees(project.root()).is_empty(), "nothing ran");
}

#[tokio::test]
async fn a_check_that_passes_either_way_does_not_discriminate() {
    #[cfg(windows)]
    let (program, args) = ("findstr", vec!["/c:e", "state.txt"]);
    #[cfg(not(windows))]
    let (program, args) = ("grep", vec!["-q", "e", "state.txt"]);
    let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
    let project = project_with(program, &args, true, false);
    let (candidate, outcome) = replay(&project, false).await;
    assert_eq!(outcome.evidence.verdict, LabVerdict::Invalid);
    assert_eq!(
        outcome.evidence.reasons,
        vec![VerdictReason::NoDiscriminatingVerifier]
    );
    outcome.evidence.validate(&candidate).unwrap();
}

#[tokio::test]
async fn the_permission_gate_still_decides_and_a_headless_ask_is_denied() {
    let project = scripted("slow");
    let default = PermissionEngine::new(Profile::Default, Vec::new());

    // Headless: the engine denies a shell command it would ask about.
    let (candidate, denied) = replay_with(
        &project,
        false,
        &default,
        Interactivity::NonInteractive,
        Duration::from_secs(60),
        &CancelSignal::new(),
    )
    .await;
    assert_eq!(denied.evidence.verdict, LabVerdict::InvalidExperiment);
    assert_eq!(
        denied.evidence.reasons,
        vec![VerdictReason::Other(PERMISSION_DENIED.to_string())]
    );
    assert_eq!(denied.receipt.arms.len(), 1, "nothing further runs");
    assert_eq!(denied.receipt.arms[0].end, "denied");
    denied.evidence.validate(&candidate).unwrap();

    // Confirmed at a prompt: the confirmation answers exactly that Ask.
    let (_, confirmed) = replay_with(
        &project,
        false,
        &default,
        Interactivity::Interactive,
        Duration::from_secs(60),
        &CancelSignal::new(),
    )
    .await;
    assert_eq!(
        confirmed.evidence.verdict,
        LabVerdict::Valid,
        "{:#?}",
        confirmed.receipt
    );
}

#[tokio::test]
async fn a_missing_program_is_an_infrastructure_failure() {
    let project = project_with("definitely-not-a-real-program-xyzzy", &[], true, false);
    let (candidate, outcome) = replay(&project, false).await;
    assert_eq!(outcome.evidence.verdict, LabVerdict::InvalidExperiment);
    assert_eq!(
        outcome.evidence.reasons,
        vec![VerdictReason::Other("InfrastructureFailure".to_string())]
    );
    assert!(outcome.receipt.arms[0].end.starts_with("not started"));
    outcome.evidence.validate(&candidate).unwrap();
    assert!(lab_worktrees(project.root()).is_empty());
}

#[tokio::test]
async fn a_timeout_is_a_breached_budget_and_the_worktree_goes() {
    let project = scripted("slow");
    let (candidate, outcome) = replay_with(
        &project,
        false,
        &bypass(),
        Interactivity::NonInteractive,
        Duration::from_secs(1),
        &CancelSignal::new(),
    )
    .await;
    assert_eq!(outcome.evidence.verdict, LabVerdict::InvalidExperiment);
    assert_eq!(
        outcome.evidence.reasons,
        vec![VerdictReason::BudgetExceeded]
    );
    assert_eq!(outcome.receipt.arms[0].end, "timed out");
    assert!(lab_worktrees(project.root()).is_empty());
    outcome.evidence.validate(&candidate).unwrap();
}

#[tokio::test]
async fn cancelling_reaps_the_whole_tree_as_its_effect_shows() {
    let project = scripted("hang");
    let heartbeat = project.root().join(".localpilot").join("heartbeat.txt");
    let cancel = CancelSignal::new();
    let trigger = {
        let cancel = cancel.clone();
        let heartbeat = heartbeat.clone();
        tokio::spawn(async move {
            // Cancel only once the grandchild is demonstrably alive.
            for _ in 0..200 {
                if heartbeat.is_file() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            cancel.cancel();
        })
    };
    let (candidate, outcome) = replay_with(
        &project,
        false,
        &bypass(),
        Interactivity::NonInteractive,
        Duration::from_secs(120),
        &cancel,
    )
    .await;
    trigger.await.unwrap();

    assert_eq!(outcome.evidence.verdict, LabVerdict::InvalidExperiment);
    assert_eq!(outcome.evidence.reasons, vec![VerdictReason::Cancelled]);
    assert!(outcome.evidence.arms[0].cancelled);
    assert!(
        heartbeat.is_file(),
        "the grandchild ran: {:#?}",
        outcome.receipt
    );
    // The effect, not an exit code: the grandchild stops writing.
    let settled = std::fs::metadata(&heartbeat).unwrap().len();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(
        std::fs::metadata(&heartbeat).unwrap().len(),
        settled,
        "a reaped tree writes nothing more"
    );
    assert!(lab_worktrees(project.root()).is_empty());
    outcome.evidence.validate(&candidate).unwrap();
}

#[tokio::test]
async fn a_flood_of_output_is_bounded_and_the_verdict_still_stands() {
    let project = scripted("flood");
    let (_, outcome) = replay(&project, false).await;
    assert_eq!(
        outcome.evidence.verdict,
        LabVerdict::Valid,
        "{:#?}",
        outcome
            .receipt
            .arms
            .iter()
            .map(|a| &a.end)
            .collect::<Vec<_>>()
    );
    for arm in &outcome.receipt.arms {
        assert!(arm.truncated, "{} MB went past the cap", 2);
        assert!(arm.output.chars().count() <= 4_000);
    }
    assert!(outcome.evidence.arms.iter().all(|arm| arm.truncated));
}

#[tokio::test]
async fn a_check_that_edits_its_own_tests_is_a_changed_oracle() {
    let project = scripted("mutate");
    let (candidate, outcome) = replay(&project, false).await;
    assert_eq!(outcome.evidence.verdict, LabVerdict::Invalid);
    assert_eq!(outcome.evidence.reasons, vec![VerdictReason::OracleMutable]);
    assert!(outcome.receipt.arms[0]
        .mutated
        .iter()
        .any(|change| change.contains("tests/users.txt")));
    outcome.evidence.validate(&candidate).unwrap();
    assert!(outcome.receipt.main_checkout_unchanged);
}

#[tokio::test]
async fn a_worktree_left_by_a_killed_run_is_removed_when_the_next_starts() {
    let project = project(false);
    let leftover =
        localpilot_patchgen::Worktree::create_at(project.root(), "lab-crashed-0", "HEAD").unwrap();
    let path = leftover.path().to_path_buf();
    // A killed process runs no cleanup.
    std::mem::forget(leftover);
    assert!(path.is_dir());

    let (_, outcome) = replay(&project, false).await;

    assert!(!path.exists(), "{:#?}", outcome.receipt.swept_worktrees);
    assert_eq!(outcome.receipt.swept_worktrees.len(), 1);
    assert!(outcome.receipt.swept_worktrees[0].ends_with("removed"));
    assert_eq!(outcome.evidence.verdict, LabVerdict::Valid);
}

#[tokio::test]
async fn a_worktree_that_cannot_be_removed_is_invalid_and_says_what_remains() {
    let project = scripted("slow");
    let root = project.root().to_path_buf();
    let held = std::sync::Arc::new(std::sync::Mutex::new(None::<PathBuf>));
    let hold = {
        let held = std::sync::Arc::clone(&held);
        std::thread::spawn(move || {
            for _ in 0..600 {
                if let Some(dir) = lab_worktrees(&root).into_iter().next() {
                    if dir.join("state.txt").is_file() {
                        let guard = pin_in_place(&dir);
                        *held.lock().unwrap() = Some(dir);
                        return Some(guard);
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            None
        })
    };

    let (candidate, outcome) = replay(&project, false).await;
    let guard = hold.join().unwrap();
    assert!(guard.is_some(), "the worktree was never seen");

    assert_eq!(
        outcome.evidence.verdict,
        LabVerdict::InvalidExperiment,
        "{:#?}",
        outcome.receipt.arms
    );
    assert!(outcome
        .evidence
        .reasons
        .contains(&VerdictReason::Other(CLEANUP_FAILED.to_string())));
    let dir = held.lock().unwrap().clone().unwrap();
    let arm = &outcome.receipt.arms[0];
    assert!(arm.cleanup.contains("remains"), "{}", arm.cleanup);
    assert!(
        outcome
            .evidence
            .limitations
            .iter()
            .any(|l| l.contains("remains")),
        "no hidden success"
    );
    outcome.evidence.validate(&candidate).unwrap();

    drop(guard);
    unpin(&dir);
    let _ = localpilot_patchgen::sweep_worktrees(project.root(), "lab-");
}

/// Keep `dir` from being removed until the guard is dropped.
#[cfg(windows)]
fn pin_in_place(dir: &Path) -> Option<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .share_mode(0)
        .open(dir.join("held.lock"))
        .ok()
}
#[cfg(not(windows))]
fn pin_in_place(dir: &Path) -> Option<std::fs::File> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).ok()?;
    std::fs::File::open(dir).ok()
}

#[cfg(windows)]
fn unpin(_dir: &Path) {}
#[cfg(not(windows))]
fn unpin(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
}

#[tokio::test]
async fn receipts_past_retention_are_swept_by_location() {
    let project = project(false);
    let runs = project.root().join(".localpilot").join("lab").join("runs");
    std::fs::create_dir_all(&runs).unwrap();
    let old = runs.join("lab-old-1.json");
    std::fs::write(&old, "{}").unwrap();
    let forty_days = std::time::SystemTime::now() - Duration::from_secs(40 * 86_400);
    std::fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(forty_days)
        .unwrap();
    let fresh = runs.join("lab-fresh-1.json");
    std::fs::write(&fresh, "{}").unwrap();

    let (_, outcome) = replay(&project, false).await;

    assert!(!old.exists());
    assert!(fresh.exists(), "within retention");
    assert_eq!(
        outcome.receipt.swept_receipts,
        vec!["lab-old-1.json".to_string()]
    );
}

#[test]
fn a_worktree_fits_the_longest_tracked_path_and_a_too_deep_root_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "test@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "core.longpaths", "false"]);
    // As long as LocalPilot's own longest tracked path.
    let long = format!("crates/{}/lib.rs", "a".repeat(129 - "crates//lib.rs".len()));
    assert_eq!(long.len(), 129);
    commit(root, &[(long.as_str(), "x\n")], "long");

    let mut worktree = localpilot_patchgen::Worktree::create_at(root, "lab-0123abcd-0", "HEAD")
        .unwrap_or_else(|error| panic!("the in-repo root fits: {error}"));
    assert!(worktree.path().join(&long).is_file());
    worktree.remove().unwrap();

    let too_deep = root.join("d".repeat(150));
    let result = localpilot_patchgen::check_path_budget(root, &too_deep, "HEAD");
    if cfg!(windows) {
        let error = result.unwrap_err().to_string();
        assert!(error.starts_with("path too long"), "{error}");
        assert!(error.contains("129"), "{error}");
    } else {
        assert!(result.is_ok(), "only Windows has this limit");
    }

    let named = localpilot_patchgen::Worktree::create_at(root, &"n".repeat(41), "HEAD");
    assert!(named.is_err(), "a generated name is bounded");
}

/// The review output: an authorized pass, then a denied run, a timeout, a
/// check that edits its own tests, and a worktree that cannot be removed, each
/// as its receipt records it. Run with `--nocapture` to print them.
#[tokio::test]
async fn review_receipts() {
    let mut rows: Vec<String> = Vec::new();
    let mut record = |scenario: &str, outcome: &ReplayOutcome| {
        let arms = outcome
            .receipt
            .arms
            .iter()
            .map(|arm| format!("{} {} ({})", arm.name, arm.end, arm.cleanup))
            .collect::<Vec<_>>()
            .join("; ");
        let reasons = outcome
            .evidence
            .reasons
            .iter()
            .map(|reason| format!("{reason:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        rows.push(format!(
            "| {scenario} | {:?}{} | {arms} |",
            outcome.evidence.verdict,
            if reasons.is_empty() {
                String::new()
            } else {
                format!(" — {reasons}")
            }
        ));
    };

    let pass = project(false);
    record("authorized fail/fix pair", &replay(&pass, false).await.1);

    let slow = scripted("slow");
    let default = PermissionEngine::new(Profile::Default, Vec::new());
    let denied = replay_with(
        &slow,
        false,
        &default,
        Interactivity::NonInteractive,
        Duration::from_secs(60),
        &CancelSignal::new(),
    )
    .await
    .1;
    record("headless, Default profile, a shell command", &denied);
    let timeout = replay_with(
        &slow,
        false,
        &bypass(),
        Interactivity::NonInteractive,
        Duration::from_secs(1),
        &CancelSignal::new(),
    )
    .await
    .1;
    record("a 1 s budget", &timeout);

    let mutate = scripted("mutate");
    record(
        "the check edits its own tests",
        &replay(&mutate, false).await.1,
    );

    let held = scripted("slow");
    let root = held.root().to_path_buf();
    let hold = std::thread::spawn(move || {
        for _ in 0..600 {
            if let Some(dir) = lab_worktrees(&root).into_iter().next() {
                if dir.join("state.txt").is_file() {
                    return pin_in_place(&dir).map(|guard| (guard, dir));
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    });
    let cleanup = replay(&held, false).await.1;
    let pinned = hold.join().unwrap();
    record("a worktree held open during removal", &cleanup);
    if let Some((guard, dir)) = pinned {
        drop(guard);
        unpin(&dir);
    }
    let _ = localpilot_patchgen::sweep_worktrees(held.root(), "lab-");

    println!("| Scenario | Verdict | Arms (end, cleanup) |");
    println!("|---|---|---|");
    for row in &rows {
        println!("{row}");
    }
    assert_eq!(rows.len(), 5);
}

#[test]
fn a_worktrees_directory_linked_elsewhere_is_refused() {
    let project = project(false);
    let elsewhere = tempfile::tempdir().unwrap();
    let dot = project.root().join(".localpilot");
    std::fs::create_dir_all(&dot).unwrap();
    let link = dot.join("worktrees");
    #[cfg(windows)]
    {
        let made = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(elsewhere.path())
            .output()
            .unwrap();
        assert!(
            made.status.success(),
            "{}",
            String::from_utf8_lossy(&made.stderr)
        );
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(elsewhere.path(), &link).unwrap();

    let refused = localpilot_patchgen::Worktree::create_at(project.root(), "lab-alias-0", "HEAD");
    let error = refused.unwrap_err().to_string();
    assert!(error.contains("outside the repository"), "{error}");
    assert!(
        std::fs::read_dir(elsewhere.path())
            .unwrap()
            .next()
            .is_none(),
        "nothing was created through the link"
    );
}
