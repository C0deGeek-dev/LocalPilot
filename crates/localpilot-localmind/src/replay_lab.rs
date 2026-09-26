//! The Replay tier: run a frozen `Replay` assignment's ratified check on the
//! commits it came from, in temporary worktrees, and judge whether the check
//! tells the broken state from the fixed one.
//!
//! Two sources reach here, both frozen by eligibility:
//!
//! - a **fail/fix pair** — the check at the step's parent revision must fail,
//!   and at the step's commit must pass;
//! - a **controlled mutation** — the check at the current revision with the
//!   step's fix reverted must fail, and at the current revision as it is must
//!   pass.
//!
//! Before anything runs, the project must allow Replay in its **committed**
//! `.localpilot.toml` (`[lab] replay = true`), the check must still be ratified
//! there with the command it ran, and the assignment's frozen oracle and
//! fixture must hash to what they hashed to when it was frozen. A preview says
//! exactly what will run; nothing runs without an explicit confirmation.
//!
//! Each arm gets its own worktree under the in-repo `.localpilot/worktrees/`,
//! detached at the exact revision. The check runs through the ordinary
//! permission-gated quality runner — never its fixer — with a fixed environment
//! allowlist, a timeout, bounded output, cancellation, and the whole-tree reap
//! the shell tool uses. The worktree is removed afterwards and the removal is
//! checked; a worktree left by a process that was killed mid-run is removed
//! when the next run starts. A worktree is not a sandbox: the check runs with
//! the user's own access to the machine, and may use the network.
//!
//! What this proves: the fixture and its oracle — that the ratified check fails
//! without the fix and passes with it. Like Logic, it replays a known repair,
//! so it can never say whether the lesson helps.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use localmind_core::{
    ArmRecord, AssignmentSource, CandidateLesson, EvidenceTier, ExperimentEvidence,
    ExperimentInputs, ExperimentProvenance, LabVerdict, LessonAssignment, LogRef, OracleOrigin,
    VerdictReason, EXPERIMENT_EVIDENCE_VERSION,
};
use localpilot_config::{CheckConfig, Config};
use localpilot_harness::{
    check_command_digest, CancelSignal, CheckRunner, CommandEnd, CommandRun, EnvPolicy,
    QUALITY_CHECK_TOOL,
};
use localpilot_patchgen::{sweep_worktrees, Worktree};
use localpilot_sandbox::{Approver, Interactivity, PermissionEngine, PermissionRequest};
use serde::{Deserialize, Serialize};

use crate::lab_eligibility::{git, is_test_path, sha256_hex, short, test_surface};

/// Every Replay worktree's name starts with this, and only those are swept.
pub const REPLAY_WORKTREE_PREFIX: &str = "lab-";
/// Where run receipts are kept, under the project's `.localpilot/`. Swept by
/// age through LocalMind's lab-retention plan.
pub const LAB_RUNS_DIR: &str = "lab/runs";
/// The build-output directory every Replay run shares, under `.localpilot/`,
/// so a run after the first builds warm without touching the project's own.
pub const LAB_TARGET_DIR: &str = "lab/target";
/// The default time each arm may take.
pub const DEFAULT_ARM_TIMEOUT: Duration = Duration::from_secs(900);

/// The arm that must fail.
pub const EXPECT_FAIL_ARM: &str = "expect-fail";
/// The arm that must pass.
pub const EXPECT_PASS_ARM: &str = "expect-pass";

/// The reason a result carries when the permission engine refused the check.
pub const PERMISSION_DENIED: &str = "PermissionDenied";
/// The reason a result carries when the run itself failed: a worktree that
/// could not be made, a check that could not start, something the check
/// started still running after it ended.
pub const INFRASTRUCTURE_FAILURE: &str = "InfrastructureFailure";
/// The reason a result carries when a worktree could not be removed.
pub const CLEANUP_FAILED: &str = "CleanupFailed";
/// The reason a result carries when the main checkout changed during a run.
pub const SOURCE_MUTATED: &str = "SourceMutated";
/// The reason a result carries when a worktree would not fit the path limit.
pub const PATH_TOO_LONG: &str = "PathTooLong";

/// Host variables a check keeps; everything else — tokens, keys, the user's
/// own settings — is dropped. Shown in the preview.
const ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "PATHEXT",
    "HOME",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "TEMP",
    "TMP",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "SystemRoot",
    "SystemDrive",
    "windir",
    "ComSpec",
    "OS",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
];

/// One side of a Replay run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayArm {
    /// [`EXPECT_FAIL_ARM`] or [`EXPECT_PASS_ARM`].
    pub name: String,
    /// The exact revision the worktree is detached at.
    pub revision: String,
    /// A commit reverted in the worktree before the check runs, for a
    /// controlled mutation's failing arm.
    pub revert: Option<String>,
    /// Whether the check must pass here.
    pub expect_pass: bool,
}

/// Everything a Replay run will do, checked and fixed before it starts. What
/// [`preview`] shows is exactly what [`run_replay`] runs.
#[derive(Clone, Debug)]
pub struct ReplayPlan {
    pub candidate_identity: String,
    pub assignment: LessonAssignment,
    /// The ratified check, as the committed configuration has it.
    pub check: CheckConfig,
    pub arms: Vec<ReplayArm>,
    /// Host variables passed through, by name.
    pub env_keep: Vec<String>,
    /// Variables set for the check.
    pub env_set: Vec<(String, String)>,
    pub timeout: Duration,
    /// The project revision the result is bound to.
    pub source_revision: String,
}

/// Why no Replay run can be planned.
#[derive(Clone, Debug, PartialEq)]
pub enum ReplayRefusal {
    /// The committed `.localpilot.toml` does not set `[lab] replay = true`.
    NotEnabled,
    /// The trust boundary cannot be read, or the working copy differs from
    /// what is committed.
    Untrusted(String),
    /// The assignment is not a fail/fix pair or a controlled mutation.
    NotReplay,
    /// The assignment was frozen for another version of the lesson.
    StaleAssignment,
    /// The assignment no longer holds: its oracle or fixture changed, or its
    /// check is no longer ratified as it ran. `Invalid` evidence says so.
    Unsound {
        reasons: Vec<VerdictReason>,
        detail: String,
    },
}

impl std::fmt::Display for ReplayRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotEnabled => write!(
                f,
                "Replay is off for this project; enable it with `[lab] replay = true` in the \
                 committed .localpilot.toml"
            ),
            Self::Untrusted(detail) => write!(f, "the project's trust boundary: {detail}"),
            Self::NotReplay => write!(f, "not a Replay assignment"),
            Self::StaleAssignment => write!(
                f,
                "the assignment was frozen for an earlier version of the lesson"
            ),
            Self::Unsound { detail, .. } => write!(f, "the assignment no longer holds: {detail}"),
        }
    }
}

impl ReplayRefusal {
    /// The evidence a refusal leaves on the lesson, when it is a finding about
    /// the assignment. `None` for a refusal that says nothing about it.
    #[must_use]
    pub fn evidence(
        &self,
        candidate: &CandidateLesson,
        assignment: &LessonAssignment,
        source_revision: &str,
    ) -> Option<ExperimentEvidence> {
        let Self::Unsound { reasons, detail } = self else {
            return None;
        };
        let mut evidence = base_evidence(
            candidate.content_identity(),
            assignment,
            source_revision,
            None,
            LabVerdict::Invalid,
            reasons.clone(),
        );
        evidence.limitations.push(bounded(detail));
        Some(evidence)
    }
}

/// Check that `assignment` can run as Replay in the project at `root`, and fix
/// exactly what will run.
///
/// # Errors
/// A [`ReplayRefusal`] saying why not.
pub fn plan_replay(
    root: &Path,
    candidate: &CandidateLesson,
    assignment: &LessonAssignment,
    timeout: Duration,
) -> Result<ReplayPlan, ReplayRefusal> {
    if assignment.candidate_identity != candidate.content_identity() {
        return Err(ReplayRefusal::StaleAssignment);
    }
    let arms = match &assignment.source {
        Some(AssignmentSource::FailFixPair {
            base_revision,
            fix_revision,
        }) => vec![
            ReplayArm {
                name: EXPECT_FAIL_ARM.to_string(),
                revision: base_revision.clone(),
                revert: None,
                expect_pass: false,
            },
            ReplayArm {
                name: EXPECT_PASS_ARM.to_string(),
                revision: fix_revision.clone(),
                revert: None,
                expect_pass: true,
            },
        ],
        Some(AssignmentSource::ControlledMutation {
            applied_to,
            repair_revision,
        }) => vec![
            ReplayArm {
                name: EXPECT_FAIL_ARM.to_string(),
                revision: applied_to.clone(),
                revert: Some(repair_revision.clone()),
                expect_pass: false,
            },
            ReplayArm {
                name: EXPECT_PASS_ARM.to_string(),
                revision: applied_to.clone(),
                revert: None,
                expect_pass: true,
            },
        ],
        _ => return Err(ReplayRefusal::NotReplay),
    };

    let config = committed_config(root)?;
    if !config.lab.replay {
        return Err(ReplayRefusal::NotEnabled);
    }

    let unsound = |reason: VerdictReason, detail: String| ReplayRefusal::Unsound {
        reasons: vec![reason],
        detail,
    };
    if assignment.oracle.origin == OracleOrigin::DerivedFromLesson {
        return Err(unsound(
            VerdictReason::OracleNotIndependent,
            "the oracle was written from the lesson".to_string(),
        ));
    }
    let name = assignment
        .verifier
        .name
        .strip_prefix("ratified-check:")
        .unwrap_or_default();
    let Some(check) = config
        .harness
        .checks
        .iter()
        .find(|check| {
            check.name == name && check_command_digest(check) == assignment.verifier.version
        })
        .cloned()
    else {
        return Err(unsound(
            VerdictReason::OracleMutable,
            format!("the check `{name}` is no longer ratified with the command it ran"),
        ));
    };
    for arm in &arms {
        let revisions = std::iter::once(arm.revision.as_str()).chain(arm.revert.as_deref());
        for revision in revisions {
            if git(
                root,
                &["rev-parse", "--verify", &format!("{revision}^{{commit}}")],
            )
            .is_none()
            {
                return Err(unsound(
                    VerdictReason::FixtureUnavailable,
                    format!("the revision {} is not in this repository", short(revision)),
                ));
            }
        }
    }

    // The oracle and the fixture, hashed the way they were frozen.
    let oracle_revision = assignment
        .oracle
        .locator
        .rsplit('@')
        .next()
        .unwrap_or_default();
    let oracle_hash = test_surface(root, oracle_revision).map(|surface| {
        sha256_hex(&format!(
            "{}\n{oracle_revision}\n{surface}",
            assignment.verifier.version
        ))
    });
    if oracle_hash.as_deref() != Some(assignment.oracle.content_hash.as_str()) {
        return Err(unsound(
            VerdictReason::OracleMutable,
            "the oracle's command or test files are not what was frozen".to_string(),
        ));
    }
    if fixture_hash(root, &arms).as_deref() != Some(assignment.fixture.content_hash.as_str()) {
        return Err(unsound(
            VerdictReason::FixtureUnavailable,
            "the fixture's revisions are not what was frozen".to_string(),
        ));
    }

    let target = root.join(".localpilot").join(LAB_TARGET_DIR);
    Ok(ReplayPlan {
        candidate_identity: candidate.content_identity(),
        assignment: assignment.clone(),
        check,
        arms,
        env_keep: ENV_ALLOWLIST
            .iter()
            .map(|name| (*name).to_string())
            .collect(),
        env_set: vec![(
            "CARGO_TARGET_DIR".to_string(),
            target.to_string_lossy().into_owned(),
        )],
        timeout,
        source_revision: crate::lab_eligibility::current_revision(root),
    })
}

/// The committed `.localpilot.toml`, as `HEAD` has it. The working copy must
/// match: an uncommitted edit could otherwise change the checks it trusts.
fn committed_config(root: &Path) -> Result<Config, ReplayRefusal> {
    let committed = git(root, &["show", "HEAD:.localpilot.toml"]).ok_or_else(|| {
        ReplayRefusal::Untrusted("no committed .localpilot.toml at HEAD".to_string())
    })?;
    let working = std::fs::read_to_string(root.join(".localpilot.toml")).unwrap_or_default();
    if normalise(&working) != normalise(&committed) {
        return Err(ReplayRefusal::Untrusted(
            ".localpilot.toml has uncommitted changes; commit or discard them first".to_string(),
        ));
    }
    toml::from_str::<Config>(&committed).map_err(|error| {
        ReplayRefusal::Untrusted(format!(
            "the committed .localpilot.toml does not parse: {error}"
        ))
    })
}

fn normalise(text: &str) -> String {
    text.replace("\r\n", "\n").trim_end().to_string()
}

/// The fixture hash as eligibility froze it.
fn fixture_hash(root: &Path, arms: &[ReplayArm]) -> Option<String> {
    let fail = arms.iter().find(|arm| !arm.expect_pass)?;
    match &fail.revert {
        None => git(root, &["rev-parse", &format!("{}^{{tree}}", fail.revision)])
            .map(|tree| format!("git-tree:{tree}")),
        Some(fix) => {
            let base = git(root, &["rev-parse", "--verify", &format!("{fix}^")])?;
            let tree = git(root, &["rev-parse", &format!("{}^{{tree}}", fail.revision)])?;
            let patch = git(root, &["diff", "--no-color", "--no-ext-diff", &base, fix])?;
            Some(sha256_hex(&format!("{tree}\n{patch}")))
        }
    }
}

/// What will run, for a person to confirm. Plain text; nothing here has run.
#[must_use]
pub fn preview(plan: &ReplayPlan) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let command = command_line(&plan.check);
    let _ = writeln!(
        out,
        "Replay for lesson {}: the ratified check `{}` runs {} time(s) in temporary worktrees.",
        short(&plan.candidate_identity),
        plan.check.name,
        plan.arms.len()
    );
    for arm in &plan.arms {
        let state = match &arm.revert {
            Some(fix) => format!("{} with {} reverted", short(&arm.revision), short(fix)),
            None => short(&arm.revision).to_string(),
        };
        let expect = if arm.expect_pass { "pass" } else { "fail" };
        let _ = writeln!(out, "  {}: at {state}, must {expect}", arm.name);
    }
    let _ = writeln!(out, "  command: {command}");
    let _ = writeln!(
        out,
        "  environment: only {} (when set), plus {}",
        plan.env_keep.join(", "),
        plan.env_set
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let _ = writeln!(
        out,
        "  budget: {} s per run; output kept up to 16 KiB per stream; no fixer runs",
        plan.timeout.as_secs()
    );
    let _ = writeln!(
        out,
        "  A worktree is not a sandbox: the check runs with your access to this machine and may \
         use the network. Your checkout is not touched; each worktree is removed afterwards."
    );
    out
}

/// A run's full account, kept under `.localpilot/lab/runs/`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayReceipt {
    pub run_id: String,
    pub candidate_identity: String,
    pub assignment_identity: String,
    pub check: String,
    pub command: String,
    pub env_names: Vec<String>,
    pub arms: Vec<ArmReceipt>,
    /// Worktrees a killed earlier run left behind, removed before this one.
    pub swept_worktrees: Vec<String>,
    /// Receipts past their retention removed before this run.
    pub swept_receipts: Vec<String>,
    pub main_checkout_unchanged: bool,
    pub verdict: LabVerdict,
    pub reasons: Vec<VerdictReason>,
}

/// One arm's account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArmReceipt {
    pub name: String,
    pub revision: String,
    pub revert: Option<String>,
    pub expect_pass: bool,
    /// How the check ended: `exited 0`, `timed out`, `cancelled`, `denied`,
    /// `not started: …`, or `not run: …` when setup failed.
    pub end: String,
    pub exit_code: Option<i32>,
    pub passed: bool,
    pub elapsed_ms: u64,
    pub truncated: bool,
    /// Whether everything the check started had let go of its output when it
    /// ended.
    pub pipes_closed: bool,
    /// Tracked files the check itself changed in the worktree.
    pub mutated: Vec<String>,
    /// Bounded, redacted output.
    pub output: String,
    pub worktree: String,
    /// `removed`, or what went wrong and what remains.
    pub cleanup: String,
}

/// A finished Replay run.
#[derive(Clone, Debug)]
pub struct ReplayOutcome {
    pub evidence: ExperimentEvidence,
    pub receipt: ReplayReceipt,
    /// Where the receipt was written, when it could be.
    pub receipt_path: Option<PathBuf>,
}

/// Run a confirmed plan. The permission engine still decides every spawn, as
/// it does for the quality gate: the person's confirmation of the preview
/// answers an `Ask` for exactly the previewed command and nothing else, and
/// never overrides a `Deny`. Run `Interactive` when the person confirmed at a
/// prompt; a headless run (`--yes`) is `NonInteractive`, where the engine turns
/// an `Ask` into a `Deny` exactly as it does for headless gate checks.
pub async fn run_replay(
    root: &Path,
    plan: &ReplayPlan,
    engine: &PermissionEngine,
    interactivity: Interactivity,
    cancel: &CancelSignal,
) -> ReplayOutcome {
    let started_at = unix_now();
    let run_id = format!(
        "{}{}-{started_at}",
        REPLAY_WORKTREE_PREFIX,
        &hex_of(&plan.assignment.identity())[..8]
    );

    // What an earlier, killed run left behind, and receipts past retention.
    let swept_worktrees: Vec<String> = sweep_worktrees(root, REPLAY_WORKTREE_PREFIX)
        .into_iter()
        .map(|(path, result)| match result {
            Ok(()) => format!("{} removed", path.display()),
            Err(error) => format!("{} could not be removed: {error}", path.display()),
        })
        .collect();
    let swept_receipts = sweep_receipts(root);

    let main_before = main_checkout_state(root);
    let approver = PreviewApprover {
        command: command_line(&plan.check),
    };
    let mut arms = Vec::new();
    let mut records = Vec::new();
    for (index, arm) in plan.arms.iter().enumerate() {
        let name = format!("{run_id}-{index}");
        let name = bounded_name(&name, index);
        let (receipt, run) = run_arm(
            root,
            plan,
            arm,
            &name,
            engine,
            &approver,
            interactivity,
            cancel,
        )
        .await;
        let stop = run
            .as_ref()
            .is_none_or(|run| !matches!(run.end, CommandEnd::Exited { .. }) || !run.pipes_closed);
        records.push(arm_record(arm, &receipt, run.as_ref()));
        arms.push(receipt);
        if stop {
            break;
        }
    }
    let main_checkout_unchanged = main_checkout_state(root) == main_before;
    let (verdict, reasons) = judge(plan, &arms, main_checkout_unchanged);

    let receipt = ReplayReceipt {
        run_id: run_id.clone(),
        candidate_identity: plan.candidate_identity.clone(),
        assignment_identity: plan.assignment.identity(),
        check: plan.check.name.clone(),
        command: command_line(&plan.check),
        env_names: plan
            .env_keep
            .iter()
            .cloned()
            .chain(plan.env_set.iter().map(|(name, _)| name.clone()))
            .collect(),
        arms,
        swept_worktrees,
        swept_receipts,
        main_checkout_unchanged,
        verdict,
        reasons: reasons.clone(),
    };
    let receipt_path = write_receipt(root, &receipt);
    if let Some(path) = &receipt_path {
        let log = log_ref(root, path, &receipt);
        for record in &mut records {
            record.logs.push(log.clone());
        }
    }

    let mut evidence = base_evidence(
        plan.candidate_identity.clone(),
        &plan.assignment,
        &plan.source_revision,
        Some(plan),
        verdict,
        reasons,
    );
    evidence.arms = records;
    for arm in &receipt.arms {
        if arm.cleanup != "removed" {
            evidence
                .limitations
                .push(bounded(&format!("{}: {}", arm.name, arm.cleanup)));
        }
    }
    if !main_checkout_unchanged {
        evidence
            .limitations
            .push("the main checkout changed while the run was in progress".to_string());
    }
    ReplayOutcome {
        evidence,
        receipt,
        receipt_path,
    }
}

/// One arm: worktree, optional revert, the check, the mutation check, removal.
#[allow(clippy::too_many_arguments)] // one arm's whole context, each part named
async fn run_arm(
    root: &Path,
    plan: &ReplayPlan,
    arm: &ReplayArm,
    name: &str,
    engine: &PermissionEngine,
    approver: &PreviewApprover,
    interactivity: Interactivity,
    cancel: &CancelSignal,
) -> (ArmReceipt, Option<CommandRun>) {
    let mut receipt = ArmReceipt {
        name: arm.name.clone(),
        revision: arm.revision.clone(),
        revert: arm.revert.clone(),
        expect_pass: arm.expect_pass,
        end: String::new(),
        exit_code: None,
        passed: false,
        elapsed_ms: 0,
        truncated: false,
        pipes_closed: true,
        mutated: Vec::new(),
        output: String::new(),
        worktree: String::new(),
        cleanup: "removed".to_string(),
    };
    let mut worktree = match Worktree::create_at(root, name, &arm.revision) {
        Ok(worktree) => worktree,
        Err(error) => {
            receipt.end = format!("not run: {error}");
            receipt.cleanup = "nothing to remove".to_string();
            return (receipt, None);
        }
    };
    receipt.worktree = worktree.path().display().to_string();
    let path = worktree.path().to_path_buf();

    let prepared = match &arm.revert {
        Some(fix) => git(&path, &["revert", "--no-commit", "--no-edit", fix])
            .map(|_| ())
            .ok_or_else(|| format!("reverting {} failed", short(fix))),
        None => Ok(()),
    };
    let run = match prepared {
        Err(detail) => {
            receipt.end = format!("not run: {detail}");
            None
        }
        Ok(()) => {
            let before = tracked_changes(&path);
            let run = CheckRunner::new(engine, approver, interactivity, true, &path)
                .with_timeout(plan.timeout)
                .with_env(EnvPolicy::Only {
                    keep: plan.env_keep.clone(),
                    set: plan.env_set.clone(),
                })
                .with_cancel(cancel.clone())
                .execute(&plan.check)
                .await;
            let after = tracked_changes(&path);
            receipt.mutated = after
                .into_iter()
                .filter(|change| !before.contains(change))
                .collect();
            receipt.end = describe(&run.end);
            if let CommandEnd::Exited { code, success } = &run.end {
                receipt.exit_code = *code;
                receipt.passed = *success;
            }
            receipt.elapsed_ms = u64::try_from(run.elapsed.as_millis()).unwrap_or(u64::MAX);
            receipt.truncated = run.truncated;
            receipt.pipes_closed = run.pipes_closed;
            receipt.output = bounded_output(&run);
            Some(run)
        }
    };

    if let Err(error) = worktree.remove() {
        let remains = path.exists().then(|| path.display().to_string());
        receipt.cleanup = match remains {
            Some(dir) => format!("removal failed ({error}); {dir} remains"),
            None => format!("removal reported an error ({error}) but nothing remains"),
        };
    } else if path.exists() {
        receipt.cleanup = format!("git removed the worktree but {} remains", path.display());
    }
    (receipt, run)
}

/// The verdict and its reasons from what the arms did.
fn judge(
    plan: &ReplayPlan,
    arms: &[ArmReceipt],
    main_checkout_unchanged: bool,
) -> (LabVerdict, Vec<VerdictReason>) {
    let other = |code: &str| VerdictReason::Other(code.to_string());
    let mut experiment = Vec::new();
    if arms
        .iter()
        .any(|arm| arm.cleanup != "removed" && arm.cleanup != "nothing to remove")
    {
        experiment.push(other(CLEANUP_FAILED));
    }
    if !main_checkout_unchanged {
        experiment.push(other(SOURCE_MUTATED));
    }
    for arm in arms {
        let reason = if arm.end == "denied" {
            Some(other(PERMISSION_DENIED))
        } else if arm.end == "cancelled" {
            Some(VerdictReason::Cancelled)
        } else if arm.end == "timed out" {
            Some(VerdictReason::BudgetExceeded)
        } else if arm.end.starts_with("not run: path too long") {
            Some(other(PATH_TOO_LONG))
        } else if arm.end.starts_with("not ") || arm.end.starts_with("failed") || !arm.pipes_closed
        {
            Some(other(INFRASTRUCTURE_FAILURE))
        } else {
            None
        };
        if let Some(reason) = reason {
            if !experiment.contains(&reason) {
                experiment.push(reason);
            }
        }
    }
    if arms.len() < plan.arms.len() && experiment.is_empty() {
        experiment.push(VerdictReason::PartialPair);
    }
    if !experiment.is_empty() {
        return (LabVerdict::InvalidExperiment, experiment);
    }

    // The check changed its own judge.
    if arms.iter().any(|arm| {
        arm.mutated
            .iter()
            .any(|change| is_test_path(change_path(change)))
    }) {
        return (LabVerdict::Invalid, vec![VerdictReason::OracleMutable]);
    }
    let discriminates = arms.iter().all(|arm| arm.passed == arm.expect_pass);
    if !discriminates {
        return (
            LabVerdict::Invalid,
            vec![VerdictReason::NoDiscriminatingVerifier],
        );
    }
    (LabVerdict::Valid, Vec::new())
}

/// Approves exactly the previewed check command, and only for the quality-check
/// identity. Consulted only on an `Ask`: a `Deny` never reaches it.
struct PreviewApprover {
    command: String,
}

impl Approver for PreviewApprover {
    fn approve<'a>(
        &'a self,
        request: &'a PermissionRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        let approved = request.tool == QUALITY_CHECK_TOOL && request.detail == self.command;
        Box::pin(async move { approved })
    }
}

fn arm_record(arm: &ReplayArm, receipt: &ArmReceipt, run: Option<&CommandRun>) -> ArmRecord {
    let observation = if receipt.passed {
        format!("the check passed at {}", short(&arm.revision))
    } else {
        format!(
            "the check did not pass at {}: {}",
            short(&arm.revision),
            receipt.end
        )
    };
    ArmRecord {
        arm: arm.name.clone(),
        attempts: u32::from(run.is_some()),
        passed: u32::from(receipt.passed),
        observations: vec![bounded(&observation)],
        logs: Vec::new(),
        wall_ms: receipt.elapsed_ms,
        truncated: receipt.truncated,
        cancelled: receipt.end == "cancelled",
    }
}

fn base_evidence(
    candidate_identity: String,
    assignment: &LessonAssignment,
    source_revision: &str,
    plan: Option<&ReplayPlan>,
    verdict: LabVerdict,
    reasons: Vec<VerdictReason>,
) -> ExperimentEvidence {
    let mut tool_versions = std::collections::BTreeMap::new();
    tool_versions.insert(
        assignment.verifier.name.clone(),
        assignment.verifier.version.clone(),
    );
    ExperimentEvidence {
        version: EXPERIMENT_EVIDENCE_VERSION,
        tier: EvidenceTier::Replay,
        inputs: ExperimentInputs {
            candidate_identity,
            assignment_identity: Some(assignment.identity()),
            source_revision: source_revision.to_string(),
            model: None,
            runtime: Some(format!("localpilot-replay/{}", env!("CARGO_PKG_VERSION"))),
            settings_digest: None,
            seed: None,
            budgets_digest: plan.map(|plan| {
                sha256_hex(&format!(
                    "timeout_secs={}\nenv={}",
                    plan.timeout.as_secs(),
                    plan.env_keep.join(",")
                ))
            }),
            tool_versions,
            validation_profile: None,
            verifier: Some(assignment.verifier.clone()),
        },
        assignment: Some(assignment.clone()),
        verdict,
        reasons,
        injection: None,
        receipt: None,
        arms: Vec::new(),
        provenance: ExperimentProvenance {
            producer: format!("localpilot-replay-lab/{}", env!("CARGO_PKG_VERSION")),
            produced_at: unix_now(),
        },
        limitations: vec![
            "Replay proves the fixture and its oracle — that the ratified check fails without \
             the fix and passes with it; it replays a known repair, so it says nothing about \
             whether the lesson helps"
                .to_string(),
        ],
    }
}

fn describe(end: &CommandEnd) -> String {
    match end {
        CommandEnd::Exited {
            code: Some(code), ..
        } => format!("exited {code}"),
        CommandEnd::Exited { code: None, .. } => "exited by signal".to_string(),
        CommandEnd::Denied => "denied".to_string(),
        CommandEnd::NotStarted(detail) => format!("not started: {detail}"),
        CommandEnd::TimedOut => "timed out".to_string(),
        CommandEnd::Cancelled => "cancelled".to_string(),
        CommandEnd::Failed(detail) => format!("failed: {detail}"),
    }
}

fn bounded_output(run: &CommandRun) -> String {
    let text = format!(
        "--- stdout ---\n{}\n--- stderr ---\n{}",
        run.stdout, run.stderr
    );
    text.chars().take(4_000).collect()
}

/// `git status --porcelain` lines for tracked files, sorted. The build output
/// goes to the shared lab target, outside the worktree.
fn tracked_changes(dir: &Path) -> Vec<String> {
    let mut lines: Vec<String> = git(dir, &["status", "--porcelain", "--untracked-files=no"])
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    lines.sort();
    lines
}

fn change_path(change: &str) -> &str {
    change.get(3..).unwrap_or(change)
}

/// The main checkout's HEAD and changes, `.localpilot/` excepted — the lab's
/// own records and worktrees live there.
fn main_checkout_state(root: &Path) -> (Option<String>, Vec<String>) {
    let head = git(root, &["rev-parse", "HEAD"]);
    let mut changes: Vec<String> = git(root, &["status", "--porcelain"])
        .unwrap_or_default()
        .lines()
        .filter(|line| {
            !change_path(line)
                .trim_matches('"')
                .starts_with(".localpilot/")
        })
        .map(str::to_string)
        .collect();
    changes.sort();
    (head, changes)
}

fn command_line(check: &CheckConfig) -> String {
    if check.args.is_empty() {
        check.program.clone()
    } else {
        format!("{} {}", check.program, check.args.join(" "))
    }
}

/// A worktree name within the patchgen bound: the run id, shortened if need
/// be, with the arm's index kept.
fn bounded_name(name: &str, index: usize) -> String {
    if name.len() <= localpilot_patchgen::MAX_WORKTREE_NAME {
        return name.to_string();
    }
    let suffix = format!("-{index}");
    let keep = localpilot_patchgen::MAX_WORKTREE_NAME - suffix.len();
    format!("{}{suffix}", &name[..keep])
}

fn runs_dir(root: &Path) -> PathBuf {
    root.join(".localpilot").join(LAB_RUNS_DIR)
}

fn write_receipt(root: &Path, receipt: &ReplayReceipt) -> Option<PathBuf> {
    let dir = runs_dir(root);
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{}.json", receipt.run_id));
    let json = serde_json::to_string_pretty(receipt).ok()?;
    std::fs::write(&path, json).ok()?;
    Some(path)
}

fn log_ref(root: &Path, path: &Path, receipt: &ReplayReceipt) -> LogRef {
    let content = std::fs::read(path).unwrap_or_default();
    let locator = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    let summary = receipt
        .arms
        .iter()
        .map(|arm| format!("{}: {}", arm.name, arm.end))
        .collect::<Vec<_>>()
        .join("; ");
    LogRef {
        locator,
        content_hash: sha256_hex(&String::from_utf8_lossy(&content)),
        bytes: u64::try_from(content.len()).unwrap_or(u64::MAX),
        summary: bounded(&summary),
        captured_at: unix_now(),
    }
}

/// Remove receipts past the lab's retention, as LocalMind plans it: only files
/// strictly inside the runs directory are ever considered.
fn sweep_receipts(root: &Path) -> Vec<String> {
    let dir = runs_dir(root);
    let Ok(dir) = dunce::canonicalize(&dir) else {
        return Vec::new();
    };
    let entries: Vec<localmind_store::SessionEntry> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let meta = entry.metadata().ok()?;
                    meta.is_file().then(|| localmind_store::SessionEntry {
                        id: entry.file_name().to_string_lossy().into_owned(),
                        path: entry.path(),
                        bytes: meta.len(),
                        modified: meta.modified().unwrap_or_else(|_| SystemTime::now()),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let plan = localmind_store::plan_lab_sweep(&dir, &entries, SystemTime::now());
    plan.retention
        .prunable
        .into_iter()
        .filter(|entry| std::fs::remove_file(&entry.path).is_ok())
        .map(|entry| entry.id)
        .collect()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

fn hex_of(identity: &str) -> String {
    let hex: String = identity.chars().filter(char::is_ascii_hexdigit).collect();
    if hex.len() >= 8 {
        hex
    } else {
        sha256_hex(identity)
            .trim_start_matches("sha256:")
            .to_string()
    }
}

fn bounded(text: &str) -> String {
    text.chars()
        .take(localmind_core::MAX_OBSERVATION_CHARS)
        .collect()
}
