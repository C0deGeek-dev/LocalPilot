//! The uplift tier's adapter: project a lesson and its approved task set into
//! a lesson-off/on A/B, stage each arm's memory, run the two arms through
//! LocalBench, and import the receipt as evidence bound to the lesson.
//!
//! LocalBench owns the measurement — the task-set format, the solver driver,
//! arm isolation, the injection contract, grading, aggregation and
//! significance. This module owns only what is the lesson's side of it:
//!
//! - **Projection.** The approved tasks and the lesson become a task-set file
//!   and a `binding` that ties the run to the candidate, its assignment, the
//!   approved oracle and the source revision.
//! - **Staging.** Both arms run in one throwaway workspace under the lab's own
//!   directory — never the project, never the user's memory. The baseline arm
//!   gets a clean store with learning off; the lesson arm gets the lesson
//!   seeded, alone, with learning on. The seeded memory's id is what the arm
//!   must then show it used.
//! - **Import.** The receipt is accepted only if every identity in it — the
//!   binding, the task set's digest, each arm's configuration and injection
//!   inputs — is the one this run asked for. A void receipt, a mismatch, half a
//!   pair, or an arm that used anything but the lesson is `InvalidExperiment`:
//!   never a finding about the lesson.
//!
//! LocalBench is reached through [`UpliftBench`], so the same orchestration
//! runs against the real CLI and against a stand-in in tests. No model is
//! called here; the solver LocalBench drives is the caller's to configure.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use localmind_core::{
    ArmRecord, AssignmentSource, CandidateLesson, EvidenceTier, ExperimentEvidence,
    ExperimentInputs, ExperimentProvenance, ImportedReceipt, InjectionMode as ProofMode,
    InjectionProof, LabVerdict, LessonAssignment, VerdictReason, EXPERIMENT_EVIDENCE_VERSION,
};
use localx_eval_core::uplift::{
    content_digest, text_digest, ArmIdentity, InjectionIdentity, InjectionMode, TaskSetIdentity,
    UpliftIdentity, UPLIFT_RECEIPT_SCHEMA,
};
use serde::{Deserialize, Serialize};

use localpilot_config::CheckConfig;
use localpilot_harness::{CancelSignal, CheckRunner, CommandEnd, CommandRun};
use localpilot_sandbox::{Approver, Interactivity, PermissionEngine};

use crate::lab_tasks::{LabTaskSet, CANDIDATE_LESSON_ID};
use crate::{SeedLesson, SeedPack};

/// Where uplift runs keep their files, under `.localpilot/`.
pub const LAB_UPLIFT_DIR: &str = "lab/uplift";

/// The reason a result carries when the receipt is not the run that was asked
/// for, or is not a receipt this build understands.
pub const RECEIPT_REJECTED: &str = "ReceiptRejected";
/// The reason a result carries when an arm could not be staged as required.
pub const MIS_STAGED: &str = "MisStaged";
/// The reason a result carries when the permission engine refused a command.
pub const PERMISSION_DENIED: &str = "PermissionDenied";
/// The reason a result carries when the benchmark tool itself failed.
pub const BENCH_FAILED: &str = "BenchFailed";

/// What an uplift run is asked to use. Fixed before the run and identical for
/// both arms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpliftSettings {
    pub model: String,
    pub trials: u32,
    /// Per-turn timeout, in seconds.
    pub timeout_secs: u64,
    /// The project revision the result is bound to.
    pub source_revision: String,
}

/// What ties a run to the lesson. Kept beside the receipt, on this side of the
/// process boundary; the receipt carries only its digest, as the binding.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Lineage {
    pub candidate_identity: String,
    pub assignment_identity: String,
    pub oracle_hash: String,
    pub source_revision: String,
}

impl Lineage {
    /// The binding a receipt must carry to belong to this lineage.
    #[must_use]
    pub fn binding(&self) -> String {
        content_digest(self)
    }
}

/// A lesson and its approved tasks, as an uplift run will see them.
#[derive(Clone, Debug)]
pub struct Projection {
    pub assignment: LessonAssignment,
    pub lineage: Lineage,
    /// The task-set file's exact content.
    pub task_set_json: String,
    pub task_set: TaskSetIdentity,
    /// The lesson the lesson arm is seeded with.
    pub lesson: String,
}

/// Why a lesson cannot be projected.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ProjectionRefusal {
    #[error("the assignment was frozen for another version of the lesson")]
    StaleAssignment,
    #[error("the assignment is not an approved task set")]
    NotAnUpliftAssignment,
    #[error("the task set has not been approved")]
    NotApproved,
    #[error("the task set is not the one the assignment froze")]
    OracleChanged,
    #[error("the task set is not valid: {0}")]
    Invalid(String),
}

/// Project `candidate` and its approved `tasks` into a run, checking the
/// assignment still binds them: it is for this version of the lesson, it came
/// from an approval, and the tasks hash to the oracle it froze.
///
/// # Errors
/// A [`ProjectionRefusal`].
pub fn project(
    candidate: &CandidateLesson,
    assignment: &LessonAssignment,
    tasks: &LabTaskSet,
    source_revision: &str,
) -> Result<Projection, ProjectionRefusal> {
    let identity = candidate.content_identity();
    if assignment.candidate_identity != identity {
        return Err(ProjectionRefusal::StaleAssignment);
    }
    if !matches!(
        assignment.source,
        Some(AssignmentSource::ApprovedTaskSet { .. })
    ) {
        return Err(ProjectionRefusal::NotAnUpliftAssignment);
    }
    if tasks.approved_by.is_none() {
        return Err(ProjectionRefusal::NotApproved);
    }
    crate::lab_tasks::validate(tasks, candidate).map_err(|problems| {
        ProjectionRefusal::Invalid(
            problems
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        )
    })?;
    if tasks.content_hash() != assignment.oracle.content_hash {
        return Err(ProjectionRefusal::OracleChanged);
    }

    let lesson = candidate.summary().to_string();
    let category = format!("{:?}", candidate.category);
    let category = if category.chars().all(|c| c.is_ascii_alphanumeric()) {
        category
    } else {
        "Process".to_string()
    };
    let short = identity.get(..12).unwrap_or(&identity).to_string();
    let task_set = serde_json::json!({
        "schema": 1,
        "name": format!("lesson-{short}"),
        "tasks": tasks.tasks.iter().map(|task| serde_json::json!({
            "id": task.id,
            "prompt": task.prompt,
            "expect": { "mode": "substring", "value": task.expect },
            "case_sensitive": false,
            "lesson_ids": [CANDIDATE_LESSON_ID],
        })).collect::<Vec<_>>(),
        "lessons": [{
            "id": CANDIDATE_LESSON_ID,
            "body": lesson,
            "category": category,
        }],
    });
    let task_set_json = serde_json::to_string_pretty(&task_set).unwrap_or_default();
    Ok(Projection {
        lineage: Lineage {
            candidate_identity: identity,
            assignment_identity: assignment.identity(),
            oracle_hash: assignment.oracle.content_hash.clone(),
            source_revision: source_revision.to_string(),
        },
        task_set: TaskSetIdentity {
            name: format!("lesson-{short}"),
            digest: text_digest(task_set_json.as_bytes()),
            task_count: tasks.tasks.len(),
        },
        assignment: assignment.clone(),
        task_set_json,
        lesson,
    })
}

/// Why a call to the benchmark tool did not succeed.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum BenchFailure {
    #[error("the permission engine refused the command")]
    Denied,
    #[error("the run was cancelled")]
    Cancelled,
    #[error("the command ran past its time limit")]
    TimedOut,
    #[error("{0}")]
    Failed(String),
}

/// One arm's invocation.
#[derive(Clone, Debug)]
pub struct ArmCall<'a> {
    pub lesson_arm: bool,
    pub task_set: &'a Path,
    pub workspace: &'a Path,
    pub settings: &'a UpliftSettings,
    pub binding: &'a str,
    /// The memory ids the lesson arm must show it used.
    pub intended: &'a [String],
    pub out: &'a Path,
}

/// The benchmark tool, as the adapter uses it. The production implementation
/// runs LocalBench through the permission-gated runner; tests supply their own.
#[async_trait(?Send)]
pub trait UpliftBench {
    /// The memory configuration an arm's workspace must be staged with.
    async fn arm_config(&self, lesson_arm: bool) -> Result<String, BenchFailure>;
    /// The seed pack the lesson arm is seeded with, for this task set.
    async fn seed_pack(&self, task_set: &Path) -> Result<String, BenchFailure>;
    /// Run one arm in its staged workspace, writing its arm file.
    async fn run_arm(&self, call: &ArmCall<'_>) -> Result<(), BenchFailure>;
    /// Join the two arm files into the receipt at `out`. A void receipt is a
    /// success here: the receipt says so.
    async fn combine(
        &self,
        baseline: &Path,
        lessons: &Path,
        out: &Path,
    ) -> Result<(), BenchFailure>;
}

/// LocalBench, run through the permission-gated check runner: every invocation
/// is presented to the permission engine like any other command, with a
/// timeout, bounded output, cancellation and the whole-tree reap.
pub struct LocalBenchCli<'a> {
    /// The `localbench` program.
    pub program: String,
    /// The `localpilot` program LocalBench drives as the solver.
    pub solver: String,
    pub engine: &'a PermissionEngine,
    pub approver: &'a dyn Approver,
    pub interactivity: Interactivity,
    pub cancel: CancelSignal,
    /// How long one arm's whole run may take.
    pub arm_timeout: std::time::Duration,
    /// The directory the commands run in.
    pub cwd: PathBuf,
}

impl LocalBenchCli<'_> {
    /// The exact command line of one invocation, for a preview.
    #[must_use]
    pub fn command_line(&self, args: &[String]) -> String {
        format!("{} {}", self.program, args.join(" "))
    }

    async fn exec(&self, args: Vec<String>, void_is_ok: bool) -> Result<CommandRun, BenchFailure> {
        let check = CheckConfig {
            name: "uplift".to_string(),
            program: self.program.clone(),
            args,
            fix_program: None,
            fix_args: Vec::new(),
            cadence: localpilot_config::Cadence::default(),
            auto_fix: localpilot_config::AutoFix::default(),
            severity: None,
        };
        let run = CheckRunner::new(
            self.engine,
            self.approver,
            self.interactivity,
            true,
            &self.cwd,
        )
        .with_timeout(self.arm_timeout)
        .with_cancel(self.cancel.clone())
        .execute(&check)
        .await;
        match &run.end {
            CommandEnd::Exited { success: true, .. } => Ok(run),
            // LocalBench exits 3 for a void receipt, which it still wrote.
            CommandEnd::Exited { code: Some(3), .. } if void_is_ok => Ok(run),
            CommandEnd::Denied => Err(BenchFailure::Denied),
            CommandEnd::Cancelled => Err(BenchFailure::Cancelled),
            CommandEnd::TimedOut => Err(BenchFailure::TimedOut),
            CommandEnd::Exited { code, .. } => Err(BenchFailure::Failed(format!(
                "localbench exited {}: {}",
                code.map_or_else(|| "by signal".to_string(), |code| code.to_string()),
                run.stderr.trim()
            ))),
            CommandEnd::NotStarted(detail) | CommandEnd::Failed(detail) => {
                Err(BenchFailure::Failed(detail.clone()))
            }
        }
    }

    /// The arguments of one arm's invocation.
    #[must_use]
    pub fn arm_args(&self, call: &ArmCall<'_>) -> Vec<String> {
        localbench_arm_args(&self.solver, call)
    }
}

/// The arguments of one arm's invocation, with `solver` as the program
/// LocalBench drives.
#[must_use]
pub fn localbench_arm_args(solver: &str, call: &ArmCall<'_>) -> Vec<String> {
    {
        let path = |path: &Path| path.to_string_lossy().into_owned();
        let mut args = vec![
            "uplift".to_string(),
            "--task-set".to_string(),
            path(call.task_set),
            "--arm".to_string(),
            if call.lesson_arm {
                "lessons"
            } else {
                "baseline"
            }
            .to_string(),
            "--workspace".to_string(),
            path(call.workspace),
            "--model".to_string(),
            call.settings.model.clone(),
            "--trials".to_string(),
            call.settings.trials.to_string(),
            "--timeout".to_string(),
            call.settings.timeout_secs.to_string(),
            "--localpilot".to_string(),
            solver.to_string(),
            "--binding".to_string(),
            call.binding.to_string(),
            "--out".to_string(),
            path(call.out),
        ];
        if call.lesson_arm {
            args.push("--intended".to_string());
            args.push(call.intended.join(","));
        }
        args
    }
}

#[async_trait(?Send)]
impl UpliftBench for LocalBenchCli<'_> {
    async fn arm_config(&self, lesson_arm: bool) -> Result<String, BenchFailure> {
        let arm = if lesson_arm { "lessons" } else { "baseline" };
        let run = self
            .exec(
                vec![
                    "uplift".to_string(),
                    "--emit-arm-config".to_string(),
                    arm.to_string(),
                ],
                false,
            )
            .await?;
        Ok(run.stdout.replace("\r\n", "\n"))
    }

    async fn seed_pack(&self, task_set: &Path) -> Result<String, BenchFailure> {
        let run = self
            .exec(
                vec![
                    "uplift".to_string(),
                    "--task-set".to_string(),
                    task_set.to_string_lossy().into_owned(),
                    "--emit-seed-pack".to_string(),
                ],
                false,
            )
            .await?;
        Ok(run.stdout.replace("\r\n", "\n"))
    }

    async fn run_arm(&self, call: &ArmCall<'_>) -> Result<(), BenchFailure> {
        self.exec(self.arm_args(call), false).await.map(|_| ())
    }

    async fn combine(
        &self,
        baseline: &Path,
        lessons: &Path,
        out: &Path,
    ) -> Result<(), BenchFailure> {
        let path = |path: &Path| path.to_string_lossy().into_owned();
        self.exec(
            vec![
                "uplift".to_string(),
                "--combine".to_string(),
                path(baseline),
                "--with".to_string(),
                path(lessons),
                "--out".to_string(),
                path(out),
            ],
            true,
        )
        .await
        .map(|_| ())
    }
}

/// A finished uplift run.
#[derive(Clone, Debug)]
pub struct UpliftOutcome {
    pub evidence: ExperimentEvidence,
    /// What the run was expected to be, for the record.
    pub expected: Option<UpliftIdentity>,
    /// The run's directory: task set, lineage, arm files and receipt.
    pub run_dir: PathBuf,
    /// Accepted memories routed to review because the lesson made things worse.
    pub flagged_for_review: Vec<String>,
    /// What the run measured about itself, apart from its result.
    pub telemetry: Telemetry,
}

/// Stage the baseline arm: a workspace with the baseline configuration and a
/// memory store that holds nothing — proven, not assumed.
///
/// # Errors
/// What could not be staged.
pub fn stage_baseline(workspace: &Path, config: &str) -> Result<(), String> {
    reset_memory(workspace)?;
    write_config(workspace, config)?;
    let held = crate::memory_list_readonly(workspace).map_err(|error| error.to_string())?;
    if !held.is_empty() {
        return Err(format!(
            "the baseline workspace's memory store holds {} lesson(s)",
            held.len()
        ));
    }
    Ok(())
}

/// Stage the lesson arm: the lesson-arm configuration, and a memory store
/// seeded with exactly the lesson under test. Returns the seeded memory's id —
/// the id the arm must show it used.
///
/// # Errors
/// The seed pack is not this lesson alone, or seeding did not leave exactly it.
pub fn stage_lessons(
    workspace: &Path,
    config: &str,
    seed_pack: &str,
    lesson: &str,
) -> Result<String, String> {
    let pack: SeedPack = serde_json::from_str(seed_pack)
        .map_err(|error| format!("the seed pack does not parse: {error}"))?;
    let [seed] = pack.lessons.as_slice() else {
        return Err(format!(
            "the seed pack holds {} lessons, not the one under test",
            pack.lessons.len()
        ));
    };
    if seed.body != lesson {
        return Err("the seed pack's lesson is not the lesson under test".to_string());
    }
    reset_memory(workspace)?;
    write_config(workspace, config)?;
    let lessons: Vec<SeedLesson> = pack.lessons;
    crate::seed_memory(workspace, &lessons, false).map_err(|error| error.to_string())?;
    let held = crate::memory_list(workspace).map_err(|error| error.to_string())?;
    match held.as_slice() {
        [only] if only.body.trim() == lesson.trim() => Ok(only.id.clone()),
        _ => Err(format!(
            "seeding left {} lesson(s) in the store, not exactly the one under test",
            held.len()
        )),
    }
}

fn reset_memory(workspace: &Path) -> Result<(), String> {
    std::fs::create_dir_all(workspace).map_err(|error| error.to_string())?;
    for name in [".localmind", ".localmind.toml"] {
        let path = workspace.join(name);
        let removed = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else if path.exists() {
            std::fs::remove_file(&path)
        } else {
            Ok(())
        };
        removed.map_err(|error| format!("{} could not be cleared: {error}", path.display()))?;
    }
    Ok(())
}

fn write_config(workspace: &Path, config: &str) -> Result<(), String> {
    std::fs::write(workspace.join(".localmind.toml"), config)
        .map_err(|error| format!("the arm configuration could not be written: {error}"))
}

/// Run the two arms for `projection` and import the receipt.
///
/// `root` is the project; everything the run writes goes under its
/// `.localpilot/lab/uplift/<run>/`, and the trial workspace there is removed
/// afterwards. `downweight` is the project's `[memory] outcome_downweight`
/// setting: when on and the lesson made things worse, an accepted memory with
/// the same text is routed to review — never deleted.
pub async fn run_uplift(
    root: &Path,
    candidate: &CandidateLesson,
    projection: &Projection,
    settings: &UpliftSettings,
    bench: &dyn UpliftBench,
    downweight: bool,
) -> UpliftOutcome {
    run_uplift_controlled(
        root,
        candidate,
        projection,
        settings,
        bench,
        downweight,
        &RunControl::default(),
    )
    .await
}

/// What bounds and steers one run. The default bounds nothing and reuses
/// nothing.
#[derive(Clone, Debug, Default)]
pub struct RunControl {
    /// The run's directory. Chosen under `.localpilot/lab/uplift/` when `None`.
    pub run_dir: Option<PathBuf>,
    /// Stops the run. The bench must honour the same signal.
    pub cancel: CancelSignal,
    /// The whole run's wall-clock ceiling. A breach cancels the run.
    pub wall: Option<std::time::Duration>,
    /// The whole run's token ceiling, read from the trial sessions while an
    /// arm runs. A breach cancels the run.
    pub max_tokens: Option<u64>,
    /// A finished baseline arm file of an identical request, to use instead of
    /// running the baseline again.
    pub reuse_baseline: Option<PathBuf>,
}

/// What a run measured about itself, apart from its result. A missing value
/// means it was not measured — never that it was zero.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Telemetry {
    pub baseline_wall_ms: Option<u64>,
    pub lessons_wall_ms: Option<u64>,
    pub total_wall_ms: u64,
    /// Input plus output tokens the trial sessions reported.
    pub tokens: Option<u64>,
    /// Whether the model was already loaded. Not measured: the endpoint is the
    /// solver's, and this adapter does not query it.
    pub model_state: Option<String>,
    /// Available memory at the start. Not measured.
    pub ram: Option<String>,
    /// GPU memory at the start. Not measured.
    pub gpu: Option<String>,
}

/// Where a run is, kept as `state.json` in its directory so its status can be
/// read without holding its process.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunState {
    pub candidate_identity: String,
    pub binding: String,
    pub model: String,
    pub trials: u32,
    pub timeout_secs: u64,
    /// `preparing`, `baseline`, `lessons`, `combining`, then `finished` (a
    /// verdict was reached) or `invalid`.
    pub stage: String,
    pub pid: u32,
    pub started_at: i64,
    pub updated_at: i64,
    pub baseline_reused: bool,
    pub verdict: Option<String>,
    pub reasons: Vec<String>,
    pub telemetry: Telemetry,
}

impl RunState {
    /// Whether the run reached an end it recorded itself.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self.stage.as_str(), "finished" | "invalid")
    }
}

/// The name of a run's state file.
pub const RUN_STATE_FILE: &str = "state.json";

/// Read a run directory's state. `None` when it has none or it does not parse.
#[must_use]
pub fn read_run_state(run_dir: &Path) -> Option<RunState> {
    serde_json::from_str(&std::fs::read_to_string(run_dir.join(RUN_STATE_FILE)).ok()?).ok()
}

fn write_run_state(run_dir: &Path, state: &RunState) {
    if let Ok(json) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(run_dir.join(RUN_STATE_FILE), json);
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// Input plus output tokens reported by every trial session in `workspace`.
/// `None` when no session reported any.
#[must_use]
pub fn workspace_tokens(workspace: &Path) -> Option<u64> {
    let entries = std::fs::read_dir(workspace.join(".localpilot").join("sessions")).ok()?;
    let mut total = None;
    for entry in entries.filter_map(Result::ok) {
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        for line in text.lines() {
            let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let kind = &event["kind"];
            if kind["type"] == "usage_reported" {
                let used = kind["input_tokens"].as_u64().unwrap_or(0)
                    + kind["output_tokens"].as_u64().unwrap_or(0);
                total = Some(total.unwrap_or(0) + used);
            }
        }
    }
    total
}

/// How often a running arm is checked against the ceilings.
const WATCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Run one arm while watching the run's ceilings. A breach cancels the run
/// through the shared signal and is returned beside the arm's own result.
async fn watched(
    arm: impl std::future::Future<Output = Result<(), BenchFailure>>,
    control: &RunControl,
    workspace: &Path,
    started: std::time::Instant,
) -> (Result<(), BenchFailure>, Option<String>) {
    tokio::pin!(arm);
    let mut breach = None;
    loop {
        tokio::select! {
            result = &mut arm => return (result, breach),
            () = tokio::time::sleep(WATCH_INTERVAL) => {
                if breach.is_some() {
                    continue;
                }
                if let Some(wall) = control.wall {
                    if started.elapsed() > wall {
                        breach = Some(format!(
                            "the run passed its wall-clock ceiling of {} s",
                            wall.as_secs()
                        ));
                    }
                }
                if let (None, Some(max)) = (&breach, control.max_tokens) {
                    if let Some(used) = workspace_tokens(workspace).filter(|used| *used > max) {
                        breach = Some(format!(
                            "the run used {used} tokens, past its ceiling of {max}"
                        ));
                    }
                }
                if breach.is_some() {
                    control.cancel.cancel();
                }
            }
        }
    }
}

/// [`run_uplift`] under a [`RunControl`]: hard ceilings that cancel on breach,
/// a state file updated at every stage, cancellation between and inside arms,
/// and an optional finished baseline to reuse.
#[allow(clippy::too_many_lines)] // one run, stage by stage, each stage's failure handled where it happens
pub async fn run_uplift_controlled(
    root: &Path,
    candidate: &CandidateLesson,
    projection: &Projection,
    settings: &UpliftSettings,
    bench: &dyn UpliftBench,
    downweight: bool,
    control: &RunControl,
) -> UpliftOutcome {
    let started = std::time::Instant::now();
    let binding = projection.lineage.binding();
    let run_dir = control.run_dir.clone().unwrap_or_else(|| {
        root.join(".localpilot")
            .join(LAB_UPLIFT_DIR)
            .join(run_name(&binding))
    });
    let workspace = run_dir.join("workspace");
    let state = std::cell::RefCell::new(RunState {
        candidate_identity: projection.lineage.candidate_identity.clone(),
        binding: binding.clone(),
        model: settings.model.clone(),
        trials: settings.trials,
        timeout_secs: settings.timeout_secs,
        stage: "preparing".to_string(),
        pid: std::process::id(),
        started_at: unix_now(),
        updated_at: unix_now(),
        baseline_reused: control.reuse_baseline.is_some(),
        verdict: None,
        reasons: Vec::new(),
        telemetry: Telemetry::default(),
    });
    let mark = |stage: &str| {
        let mut state = state.borrow_mut();
        state.stage = stage.to_string();
        state.updated_at = unix_now();
        write_run_state(&run_dir, &state);
    };
    let invalid = |reason: VerdictReason, detail: String, expected: Option<UpliftIdentity>| {
        let mut evidence = base_evidence(projection, settings);
        evidence.verdict = LabVerdict::InvalidExperiment;
        evidence.reasons = vec![reason];
        evidence.limitations.push(bounded(&detail));
        UpliftOutcome {
            evidence,
            expected,
            run_dir: run_dir.clone(),
            flagged_for_review: Vec::new(),
            telemetry: Telemetry::default(),
        }
    };
    let other = |code: &str| VerdictReason::Other(code.to_string());
    // A breach is why the run was cancelled: it is the budget, not the person.
    let failure = |failure: BenchFailure, breach: Option<String>, during: &str| match breach {
        Some(breach) => (VerdictReason::BudgetExceeded, format!("{during}: {breach}")),
        None => {
            let reason = match &failure {
                BenchFailure::Denied => other(PERMISSION_DENIED),
                BenchFailure::Cancelled => VerdictReason::Cancelled,
                BenchFailure::TimedOut => VerdictReason::BudgetExceeded,
                BenchFailure::Failed(_) => other(BENCH_FAILED),
            };
            (reason, format!("{during}: {failure}"))
        }
    };
    let done = |outcome: UpliftOutcome| finish(outcome, &workspace, &run_dir, &state, started);

    let task_set_path = run_dir.join("task-set.json");
    let prepared = std::fs::create_dir_all(&workspace)
        .and_then(|()| std::fs::write(&task_set_path, &projection.task_set_json))
        .and_then(|()| {
            let lineage =
                serde_json::to_string_pretty(&projection.lineage).map_err(std::io::Error::other)?;
            std::fs::write(run_dir.join("lineage.json"), lineage)
        });
    if let Err(error) = prepared {
        return invalid(
            other(BENCH_FAILED),
            format!("the run directory could not be prepared: {error}"),
            None,
        );
    }
    mark("preparing");

    // What the receipt must attest, gathered before either arm runs.
    let (baseline_config, lessons_config, seed_pack) = match async {
        Ok::<_, BenchFailure>((
            bench.arm_config(false).await?,
            bench.arm_config(true).await?,
            bench.seed_pack(&task_set_path).await?,
        ))
    }
    .await
    {
        Ok(parts) => parts,
        Err(error) => {
            let (reason, detail) = failure(error, None, "preparing the arms");
            return done(invalid(reason, detail, None));
        }
    };

    // Baseline: a clean store, learning off — or a finished one, reused.
    let baseline_out = run_dir.join("baseline.json");
    if let Some(finished) = &control.reuse_baseline {
        if let Err(error) = std::fs::copy(finished, &baseline_out) {
            return done(invalid(
                other(BENCH_FAILED),
                format!("the finished baseline could not be reused: {error}"),
                None,
            ));
        }
    } else {
        if control.cancel.is_cancelled() {
            return done(invalid(
                VerdictReason::Cancelled,
                "cancelled before the baseline arm".to_string(),
                None,
            ));
        }
        if let Err(detail) = stage_baseline(&workspace, &baseline_config) {
            return done(invalid(other(MIS_STAGED), detail, None));
        }
        mark("baseline");
        let call = ArmCall {
            lesson_arm: false,
            task_set: &task_set_path,
            workspace: &workspace,
            settings,
            binding: &binding,
            intended: &[],
            out: &baseline_out,
        };
        let arm_started = std::time::Instant::now();
        let (result, breach) = watched(bench.run_arm(&call), control, &workspace, started).await;
        state.borrow_mut().telemetry.baseline_wall_ms = Some(millis(arm_started));
        match (result, breach) {
            (Ok(()), None) => {}
            (Ok(()), Some(breach)) => {
                return done(invalid(
                    VerdictReason::BudgetExceeded,
                    format!("the baseline arm: {breach}"),
                    None,
                ));
            }
            (Err(error), breach) => {
                let (reason, detail) = failure(error, breach, "the baseline arm");
                return done(invalid(reason, detail, None));
            }
        }
    }

    // Between the arms: the baseline exists and the lesson arm does not.
    if control.cancel.is_cancelled() {
        let mut outcome = invalid(
            VerdictReason::Cancelled,
            "cancelled between the arms".to_string(),
            None,
        );
        outcome.evidence.reasons.push(VerdictReason::PartialPair);
        return done(outcome);
    }

    // Lesson arm: the lesson seeded, alone, learning on.
    let intended = match stage_lessons(&workspace, &lessons_config, &seed_pack, &projection.lesson)
    {
        Ok(id) => vec![id],
        Err(detail) => return done(invalid(other(MIS_STAGED), detail, None)),
    };
    let expected = UpliftIdentity {
        binding: binding.clone(),
        task_set: projection.task_set.clone(),
        baseline: arm_identity(false, &baseline_config, settings, InjectionIdentity::none()),
        lessons: arm_identity(
            true,
            &lessons_config,
            settings,
            InjectionIdentity::lessons(
                InjectionMode::Retrieved,
                intended.clone(),
                text_digest(seed_pack.trim_end().as_bytes()),
            ),
        ),
    };
    mark("lessons");
    let lessons_out = run_dir.join("lessons.json");
    let call = ArmCall {
        lesson_arm: true,
        task_set: &task_set_path,
        workspace: &workspace,
        settings,
        binding: &binding,
        intended: &intended,
        out: &lessons_out,
    };
    let arm_started = std::time::Instant::now();
    let (result, breach) = watched(bench.run_arm(&call), control, &workspace, started).await;
    state.borrow_mut().telemetry.lessons_wall_ms = Some(millis(arm_started));
    let stopped = match (result, breach) {
        (Ok(()), None) => None,
        (Ok(()), Some(breach)) => Some((
            VerdictReason::BudgetExceeded,
            format!("the lesson arm: {breach}"),
        )),
        (Err(error), breach) => Some(failure(error, breach, "the lesson arm")),
    };
    if let Some((reason, detail)) = stopped {
        // The baseline ran and the lesson arm did not finish: half a pair.
        let mut outcome = invalid(reason, detail, Some(expected));
        outcome.evidence.reasons.push(VerdictReason::PartialPair);
        return done(outcome);
    }

    mark("combining");
    let receipt_path = run_dir.join("receipt.json");
    if let Err(error) = bench
        .combine(&baseline_out, &lessons_out, &receipt_path)
        .await
    {
        let (reason, detail) = failure(error, None, "combining the arms");
        return done(invalid(reason, detail, Some(expected)));
    }
    let payload = match std::fs::read_to_string(&receipt_path) {
        Ok(payload) => payload,
        Err(error) => {
            return done(invalid(
                other(RECEIPT_REJECTED),
                format!("the receipt could not be read: {error}"),
                Some(expected),
            ))
        }
    };

    let mut evidence = base_evidence(projection, settings);
    let mut flagged = Vec::new();
    match import_receipt(&payload, &expected) {
        Ok(imported) => {
            evidence.verdict = imported.verdict;
            evidence.reasons = imported.reasons;
            evidence.injection = Some(imported.injection);
            evidence.receipt = Some(imported.receipt);
            evidence.arms = imported.arms;
            if imported.verdict == LabVerdict::Contradicted {
                flagged = recommend_review(root, candidate, downweight);
                if !flagged.is_empty() {
                    evidence.limitations.push(bounded(&format!(
                        "the accepted memory holding this lesson was routed to review: {}",
                        flagged.join(", ")
                    )));
                }
            }
        }
        Err(refusal) => {
            evidence.verdict = LabVerdict::InvalidExperiment;
            evidence.reasons = vec![other(RECEIPT_REJECTED)];
            evidence.limitations.push(bounded(&refusal.to_string()));
        }
    }
    done(UpliftOutcome {
        evidence,
        expected: Some(expected),
        run_dir: run_dir.clone(),
        flagged_for_review: flagged,
        telemetry: Telemetry::default(),
    })
}

fn millis(since: std::time::Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Close a run: take its telemetry, remove the trial workspace (saying so when
/// it will not go), and record how it ended in its state file.
fn finish(
    mut outcome: UpliftOutcome,
    workspace: &Path,
    run_dir: &Path,
    state: &std::cell::RefCell<RunState>,
    started: std::time::Instant,
) -> UpliftOutcome {
    let mut state = state.borrow_mut();
    state.telemetry.total_wall_ms = millis(started);
    state.telemetry.tokens = workspace_tokens(workspace);
    outcome.telemetry = state.telemetry.clone();
    for (arm, wall) in outcome.evidence.arms.iter_mut().zip([
        state.telemetry.baseline_wall_ms,
        state.telemetry.lessons_wall_ms,
    ]) {
        arm.wall_ms = wall.unwrap_or(0);
    }
    if workspace.exists() && std::fs::remove_dir_all(workspace).is_err() {
        outcome.evidence.limitations.push(bounded(&format!(
            "the trial workspace {} could not be removed",
            workspace.display()
        )));
    }
    state.stage = if outcome.evidence.verdict == LabVerdict::InvalidExperiment {
        "invalid"
    } else {
        "finished"
    }
    .to_string();
    state.verdict = Some(format!("{:?}", outcome.evidence.verdict));
    state.reasons = outcome
        .evidence
        .reasons
        .iter()
        .map(|reason| format!("{reason:?}"))
        .collect();
    state.updated_at = unix_now();
    write_run_state(run_dir, &state);
    outcome
}

pub(crate) fn run_name(binding: &str) -> String {
    let hex = binding.trim_start_matches("sha256:");
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    format!(
        "{}-{millis:x}-{:x}",
        hex.get(..12).unwrap_or(hex),
        std::process::id()
    )
}

fn arm_identity(
    lesson_arm: bool,
    config: &str,
    settings: &UpliftSettings,
    injection: InjectionIdentity,
) -> ArmIdentity {
    ArmIdentity {
        arm: if lesson_arm { "lessons" } else { "baseline" }.to_string(),
        is_lesson_arm: lesson_arm,
        config_digest: text_digest(config.replace("\r\n", "\n").as_bytes()),
        model: settings.model.clone(),
        trials: settings.trials,
        timeout_secs: settings.timeout_secs,
        injection,
    }
}

/// A receipt accepted as the requested run, in the lab's terms.
#[derive(Clone, Debug, PartialEq)]
pub struct Imported {
    pub verdict: LabVerdict,
    pub reasons: Vec<VerdictReason>,
    pub injection: InjectionProof,
    pub receipt: ImportedReceipt,
    pub arms: Vec<ArmRecord>,
}

/// Why a receipt was not accepted.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ImportRefusal {
    #[error("the receipt does not parse: {0}")]
    Unreadable(String),
    #[error("the receipt's schema is {0}, not {UPLIFT_RECEIPT_SCHEMA}")]
    Schema(String),
    #[error("the receipt's run id is not the digest of its own identity")]
    RunId,
    #[error("the receipt is not the requested run: {0}")]
    Mismatch(String),
    #[error("the receipt carries neither a result nor a void reason, or both")]
    Incoherent,
}

#[derive(Deserialize)]
struct Receipt {
    run_id: String,
    identity: UpliftIdentity,
    arms: Vec<ReceiptArm>,
    uplift: Option<ReceiptUplift>,
    void: Option<String>,
}

#[derive(Deserialize)]
struct ReceiptArm {
    arm: String,
    is_lesson_arm: bool,
    trials: u32,
    task_count: u32,
    per_trial_success_rate: Vec<f64>,
    mean: f64,
    injection: Option<ReceiptInjection>,
}

#[derive(Deserialize)]
struct ReceiptInjection {
    injected: Vec<String>,
}

#[derive(Deserialize)]
struct ReceiptUplift {
    verdict: String,
    delta: f64,
    band: f64,
}

/// Accept `payload` as the receipt of the `expected` run, and read its verdict.
///
/// Matching is by identity — binding, task-set digest, each arm's
/// configuration and injection inputs — never by a task-set name. A void
/// receipt is accepted as the run and reads as `InvalidExperiment`; so does a
/// lesson arm that used any memory other than the lesson under test.
///
/// # Errors
/// An [`ImportRefusal`]: the payload is not a receipt of this run.
pub fn import_receipt(payload: &str, expected: &UpliftIdentity) -> Result<Imported, ImportRefusal> {
    let schema = serde_json::from_str::<serde_json::Value>(payload)
        .map_err(|error| ImportRefusal::Unreadable(error.to_string()))?
        .get("schema")
        .map(|schema| match schema.as_str() {
            Some(tag) => tag.to_string(),
            None => schema.to_string(),
        })
        .unwrap_or_default();
    if schema != UPLIFT_RECEIPT_SCHEMA {
        return Err(ImportRefusal::Schema(schema));
    }
    let receipt: Receipt = serde_json::from_str(payload)
        .map_err(|error| ImportRefusal::Unreadable(error.to_string()))?;
    if receipt.run_id != receipt.identity.run_id() {
        return Err(ImportRefusal::RunId);
    }
    let mismatches = receipt.identity.mismatches(expected);
    if !mismatches.is_empty() {
        return Err(ImportRefusal::Mismatch(
            mismatches
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }

    let arms: Vec<ArmRecord> = receipt
        .arms
        .iter()
        .map(|arm| {
            let attempts = arm.trials.saturating_mul(arm.task_count);
            let passed: f64 = arm
                .per_trial_success_rate
                .iter()
                .map(|rate| rate * f64::from(arm.task_count))
                .sum();
            ArmRecord {
                arm: arm.arm.clone(),
                attempts,
                // A count of graded answers: never negative, never above attempts.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                passed: (passed.round().max(0.0) as u32).min(attempts),
                observations: vec![format!(
                    "mean success {:.0}% over {} trial(s) of {} task(s)",
                    arm.mean * 100.0,
                    arm.trials,
                    arm.task_count
                )],
                logs: Vec::new(),
                wall_ms: 0,
                truncated: false,
                cancelled: false,
            }
        })
        .collect();
    let stored = ImportedReceipt::new(UPLIFT_RECEIPT_SCHEMA, payload);
    let proof = |passed| InjectionProof {
        // LocalPilot injects a lesson only through retrieval.
        mode: ProofMode::Retrieved,
        assertion_passed: passed,
    };

    match (&receipt.uplift, &receipt.void) {
        (None, Some(_)) => {
            // Which arm broke the contract: the one with no injection summary.
            let baseline_broke = receipt
                .arms
                .iter()
                .any(|arm| !arm.is_lesson_arm && arm.injection.is_none());
            let lessons_broke = receipt
                .arms
                .iter()
                .any(|arm| arm.is_lesson_arm && arm.injection.is_none());
            let mut reasons = Vec::new();
            if baseline_broke {
                reasons.push(VerdictReason::ArmContaminated);
            }
            if lessons_broke || !baseline_broke {
                reasons.push(VerdictReason::InjectionNotObserved);
            }
            Ok(Imported {
                verdict: LabVerdict::InvalidExperiment,
                reasons,
                injection: proof(false),
                receipt: stored,
                arms,
            })
        }
        (Some(uplift), None) => {
            // The lesson arm must have used the lesson and nothing else.
            let intended = &expected.lessons.injection.intended;
            let only_the_lesson = receipt
                .arms
                .iter()
                .filter(|arm| arm.is_lesson_arm)
                .filter_map(|arm| arm.injection.as_ref())
                .all(|injection| injection.injected.iter().all(|id| intended.contains(id)));
            if !only_the_lesson {
                return Ok(Imported {
                    verdict: LabVerdict::InvalidExperiment,
                    reasons: vec![VerdictReason::ArmContaminated],
                    injection: proof(false),
                    receipt: stored,
                    arms,
                });
            }
            let verdict = match uplift.verdict.as_str() {
                "uplift" => LabVerdict::Supported,
                "regression" => LabVerdict::Contradicted,
                "no-effect" => LabVerdict::Inconclusive,
                other => return Err(ImportRefusal::Unreadable(format!("verdict {other:?}"))),
            };
            let mut arms = arms;
            if let Some(last) = arms.last_mut() {
                last.observations.push(format!(
                    "delta {:+.0}% against a noise band of ±{:.0}%",
                    uplift.delta * 100.0,
                    uplift.band * 100.0
                ));
            }
            Ok(Imported {
                verdict,
                reasons: Vec::new(),
                injection: proof(true),
                receipt: stored,
                arms,
            })
        }
        _ => Err(ImportRefusal::Incoherent),
    }
}

/// When the lesson made things worse and the project asks for it, route the
/// accepted memory holding the same lesson to review. A lesson still in review
/// has no accepted memory: its `Contradicted` result is the recommendation.
fn recommend_review(root: &Path, candidate: &CandidateLesson, enabled: bool) -> Vec<String> {
    if !enabled {
        return Vec::new();
    }
    let wanted = normalise(candidate.summary());
    let memories_used = crate::memory_list_readonly(root)
        .unwrap_or_default()
        .into_iter()
        .filter(|memory| normalise(&memory.body) == wanted)
        .map(|memory| localpilot_store::MemoryUsed {
            id: memory.id,
            score: 0,
            layer: "memory".to_string(),
        })
        .collect();
    let outcome = crate::UpliftArmOutcome::new(true, memories_used);
    crate::downweight_unhelpful_lessons(root, &outcome, enabled).unwrap_or_default()
}

fn normalise(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn base_evidence(projection: &Projection, settings: &UpliftSettings) -> ExperimentEvidence {
    let mut tool_versions = BTreeMap::new();
    tool_versions.insert(
        projection.assignment.verifier.name.clone(),
        projection.assignment.verifier.version.clone(),
    );
    ExperimentEvidence {
        version: EXPERIMENT_EVIDENCE_VERSION,
        tier: EvidenceTier::Uplift,
        inputs: ExperimentInputs {
            candidate_identity: projection.lineage.candidate_identity.clone(),
            assignment_identity: Some(projection.lineage.assignment_identity.clone()),
            source_revision: projection.lineage.source_revision.clone(),
            model: Some(settings.model.clone()),
            runtime: Some(format!("localpilot-uplift/{}", env!("CARGO_PKG_VERSION"))),
            settings_digest: Some(content_digest(&(settings.trials, settings.timeout_secs))),
            seed: None,
            budgets_digest: None,
            tool_versions,
            validation_profile: None,
            verifier: Some(projection.assignment.verifier.clone()),
        },
        assignment: Some(projection.assignment.clone()),
        verdict: LabVerdict::InvalidExperiment,
        reasons: Vec::new(),
        injection: None,
        receipt: None,
        arms: Vec::new(),
        provenance: ExperimentProvenance {
            producer: format!("localpilot-uplift-lab/{}", env!("CARGO_PKG_VERSION")),
            produced_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| {
                    i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
                }),
        },
        limitations: vec![
            "the lesson reached the lesson arm through retrieval, so a result within noise may \
             reflect retrieval as much as the lesson"
                .to_string(),
        ],
    }
}

fn bounded(text: &str) -> String {
    text.chars()
        .take(localmind_core::MAX_OBSERVATION_CHARS)
        .collect()
}
