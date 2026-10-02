//! Starting an uplift run: authorization, ceilings, serial execution, status
//! and restart.
//!
//! An uplift run drives real model sessions, so nothing starts one but a
//! person. The project must enable uplift in its **committed**
//! `.localpilot.toml` (`[lab] uplift = true`); the lesson must have an approved
//! task set; and the run is shown first — what is staged, the exact commands,
//! the model, and the ceilings — and runs only once confirmed. The confirmation
//! answers an `Ask` for exactly the previewed commands and never a `Deny`.
//!
//! The ceilings are declared before the run and cancel it on breach: trials,
//! tasks and the per-turn timeout bound the work jointly; a wall-clock ceiling
//! and a token ceiling bound the whole run and are watched while an arm runs. A
//! breached run is `InvalidExperiment`, never a partial verdict.
//!
//! Runs are serial within a project, and each keeps a state file, so a run's
//! status can be read without holding its process. A run that stopped after its
//! baseline finished is invalid; a later run with identical inputs is *offered*
//! that baseline, and reuses it only when told to. Nothing is queued, and
//! nothing starts by itself.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use localmind_core::{CandidateLesson, LessonAssignment};
use localpilot_harness::{CancelSignal, QUALITY_CHECK_TOOL};
use localpilot_sandbox::{Approver, Interactivity, PermissionEngine, PermissionRequest};

use crate::lab_tasks::MAX_TASKS;
use crate::replay_lab::{committed_config, resolves_inside, ReplayLock, ReplayRefusal, LOCK_STALE};
use crate::uplift_lab::{
    localbench_arm_args, project, read_run_state, run_uplift_controlled, ArmCall, LocalBenchCli,
    Projection, ProjectionRefusal, RunControl, RunState, UpliftBench, UpliftOutcome,
    UpliftSettings, LAB_UPLIFT_DIR,
};

/// The name of the lock that makes uplift runs serial, under `.localpilot/lab/`.
const UPLIFT_LOCK: &str = "uplift.lock";
/// What the preview shows where the seeded memory's id will go.
const SEEDED_ID: &str = "<seeded-id>";

/// The hard ceilings of one run, declared before it starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UpliftCeilings {
    /// Trials per task, per arm.
    pub trials: u32,
    /// Seconds one turn may take. Zero is no limit, and the default: a local
    /// model can need minutes for one turn, so the wall clock and the token
    /// ceiling bound the run instead.
    pub turn_timeout_secs: u64,
    /// Seconds the whole run may take.
    pub wall_secs: u64,
    /// Tokens the whole run may use.
    pub max_tokens: u64,
}

impl Default for UpliftCeilings {
    fn default() -> Self {
        Self {
            trials: 3,
            turn_timeout_secs: 0,
            wall_secs: 30 * 60,
            max_tokens: 400_000,
        }
    }
}

impl UpliftCeilings {
    /// How many model turns the run makes: every task, every trial, both arms.
    #[must_use]
    pub fn turns(&self, tasks: usize) -> u64 {
        u64::try_from(tasks)
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::from(self.trials))
            .saturating_mul(2)
    }

    /// The longest the work itself could take: every turn of both arms running
    /// to its timeout. `None` when turns have no limit.
    #[must_use]
    pub fn worst_case_secs(&self, tasks: usize) -> Option<u64> {
        (self.turn_timeout_secs > 0)
            .then(|| self.turns(tasks).saturating_mul(self.turn_timeout_secs))
    }
}

/// The programs a run starts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpliftTools {
    /// The `localbench` program, from the user's own configuration.
    pub localbench: String,
    /// The `localpilot` program LocalBench drives as the solver.
    pub solver: String,
}

/// A finished baseline of an identical request, left by a run that stopped.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResumeOffer {
    pub run_dir: PathBuf,
    pub baseline: PathBuf,
    /// How that run ended, for the person deciding.
    pub ended: String,
}

/// Everything a run will do, fixed before it starts. What [`authorization`]
/// shows is what runs.
#[derive(Clone, Debug)]
pub struct UpliftPlan {
    pub projection: Projection,
    pub settings: UpliftSettings,
    pub ceilings: UpliftCeilings,
    pub tools: UpliftTools,
    pub run_dir: PathBuf,
    /// The exact command lines, in order. The lesson arm's `--intended` value
    /// is the seeded memory's id, known only once the arm is staged.
    pub commands: Vec<String>,
    pub tasks: usize,
    pub resume: Option<ResumeOffer>,
}

/// Why no run can be planned or started.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum UpliftRefusal {
    #[error(
        "uplift is off for this project; enable it with `[lab] uplift = true` in the committed \
         .localpilot.toml"
    )]
    NotEnabled,
    #[error("the project's trust boundary: {0}")]
    Untrusted(String),
    #[error("the lesson has no approved task set; draft and approve one first (`lab tasks`)")]
    NoApprovedTasks,
    #[error("the approved task set no longer holds: {0}")]
    TasksInvalid(String),
    #[error("{0}")]
    Projection(#[from] ProjectionRefusal),
    #[error("a ceiling is not usable: {0}")]
    Ceiling(String),
    #[error("another uplift run is in progress: {0}")]
    Busy(String),
}

/// Check a run may be planned for `candidate` in the project at `root`, and fix
/// exactly what it will do.
///
/// # Errors
/// An [`UpliftRefusal`].
pub fn plan_uplift(
    root: &Path,
    candidate: &CandidateLesson,
    assignment: &LessonAssignment,
    model: &str,
    ceilings: UpliftCeilings,
    tools: UpliftTools,
) -> Result<UpliftPlan, UpliftRefusal> {
    let config = committed_config(root).map_err(|refusal| match refusal {
        ReplayRefusal::Untrusted(detail) => UpliftRefusal::Untrusted(detail),
        other => UpliftRefusal::Untrusted(other.to_string()),
    })?;
    if !config.lab.uplift {
        return Err(UpliftRefusal::NotEnabled);
    }
    if ceilings.trials == 0 || ceilings.wall_secs == 0 || ceilings.max_tokens == 0 {
        return Err(UpliftRefusal::Ceiling(
            "every ceiling must be above zero".to_string(),
        ));
    }
    let localpilot_dir = root.join(".localpilot");
    let tasks = crate::lab_tasks::approved_tasks(&localpilot_dir, candidate)
        .map_err(UpliftRefusal::TasksInvalid)?
        .ok_or(UpliftRefusal::NoApprovedTasks)?;
    if tasks.tasks.len() > MAX_TASKS {
        return Err(UpliftRefusal::Ceiling(format!(
            "the task set holds {} tasks, above the ceiling of {MAX_TASKS}",
            tasks.tasks.len()
        )));
    }
    let settings = UpliftSettings {
        model: model.to_string(),
        trials: ceilings.trials,
        timeout_secs: ceilings.turn_timeout_secs,
        source_revision: crate::lab_eligibility::current_revision(root),
    };
    let projection = project(candidate, assignment, &tasks, &settings.source_revision)?;
    let binding = projection.lineage.binding();
    let run_dir = localpilot_dir
        .join(LAB_UPLIFT_DIR)
        .join(crate::uplift_lab::run_name(&binding));

    let path = |path: PathBuf| path.to_string_lossy().into_owned();
    let line = |args: Vec<String>| format!("{} {}", tools.localbench, args.join(" "));
    let task_set = run_dir.join("task-set.json");
    let workspace = run_dir.join("workspace");
    let arm = |lesson_arm: bool, out: &str| {
        let out = run_dir.join(out);
        let intended = [SEEDED_ID.to_string()];
        line(localbench_arm_args(
            &tools.solver,
            &ArmCall {
                lesson_arm,
                task_set: &task_set,
                workspace: &workspace,
                settings: &settings,
                binding: &binding,
                intended: &intended,
                out: &out,
            },
        ))
    };
    let commands = vec![
        line(vec![
            "uplift".into(),
            "--emit-arm-config".into(),
            "baseline".into(),
        ]),
        line(vec![
            "uplift".into(),
            "--emit-arm-config".into(),
            "lessons".into(),
        ]),
        line(vec![
            "uplift".into(),
            "--task-set".into(),
            path(task_set.clone()),
            "--emit-seed-pack".into(),
        ]),
        arm(false, "baseline.json"),
        arm(true, "lessons.json"),
        line(vec![
            "uplift".into(),
            "--combine".into(),
            path(run_dir.join("baseline.json")),
            "--with".into(),
            path(run_dir.join("lessons.json")),
            "--out".into(),
            path(run_dir.join("receipt.json")),
        ]),
    ];
    Ok(UpliftPlan {
        resume: find_resume(root, &binding, &settings),
        tasks: tasks.tasks.len(),
        projection,
        settings,
        ceilings,
        tools,
        run_dir,
        commands,
    })
}

/// What a person is asked to authorize. Plain text; nothing here has run.
#[must_use]
pub fn authorization(plan: &UpliftPlan) -> String {
    use std::fmt::Write as _;
    let ceilings = &plan.ceilings;
    let turns = plan.tasks * ceilings.trials as usize * 2;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Uplift run for lesson {}: {} approved task(s), with and without the lesson.",
        plan.projection.lineage.candidate_identity, plan.tasks
    );
    let _ = writeln!(
        out,
        "  This drives real model sessions: {turns} turns of `{}` with model `{}`, on the \
         endpoint that program is configured to use. It may take a while and may cost tokens.",
        plan.tools.solver, plan.settings.model
    );
    let _ = writeln!(
        out,
        "  Staged in a throwaway workspace under {}: first a clean memory store with learning \
         off, then only this lesson with learning on. Your project and your own memory are not \
         touched.",
        plan.run_dir.display()
    );
    let _ = writeln!(out, "  Ceilings — the run is cancelled when one is passed:");
    match ceilings.worst_case_secs(plan.tasks) {
        Some(worst) => {
            let _ = writeln!(
                out,
                "    work: {} task(s) x {} trial(s) x 2 arms x {} s per turn = at most {} of model \
                 time",
                plan.tasks,
                ceilings.trials,
                ceilings.turn_timeout_secs,
                minutes(worst)
            );
        }
        None => {
            let _ = writeln!(
                out,
                "    work: {} task(s) x {} trial(s) x 2 arms = {} model turn(s), with no limit per \
                 turn (a local model may need minutes; set one with --turn-timeout)",
                plan.tasks,
                ceilings.trials,
                ceilings.turns(plan.tasks)
            );
        }
    }
    let _ = writeln!(
        out,
        "    wall clock: {} for the whole run",
        minutes(ceilings.wall_secs)
    );
    let _ = writeln!(out, "    tokens: {} for the whole run", ceilings.max_tokens);
    let _ = writeln!(
        out,
        "  Commands, each through the permission engine, with your environment and your access to this machine:"
    );
    for command in &plan.commands {
        let _ = writeln!(out, "    {command}");
    }
    if let Some(resume) = &plan.resume {
        let _ = writeln!(
            out,
            "  An earlier run of exactly this request stopped after its baseline arm finished \
             ({}). That baseline can be reused instead of running it again: {}",
            resume.ended,
            resume.baseline.display()
        );
    }
    out
}

fn minutes(secs: u64) -> String {
    if secs % 60 == 0 {
        format!("{} min", secs / 60)
    } else {
        format!("{} min {} s", secs / 60, secs % 60)
    }
}

/// Approves exactly the previewed commands, for the quality-check identity
/// only. Consulted only on an `Ask`: a `Deny` never reaches it.
pub struct PreviewedCommands {
    commands: Vec<String>,
}

impl PreviewedCommands {
    /// The approver for a plan's commands.
    #[must_use]
    pub fn of(plan: &UpliftPlan) -> Self {
        Self {
            commands: plan.commands.clone(),
        }
    }
}

/// The command with the seeded memory's id replaced by its placeholder.
fn normalised(command: &str) -> String {
    let mut parts: Vec<&str> = command.split(' ').collect();
    if let Some(position) = parts.iter().position(|part| *part == "--intended") {
        if let Some(value) = parts.get_mut(position + 1) {
            *value = SEEDED_ID;
        }
    }
    parts.join(" ")
}

impl Approver for PreviewedCommands {
    fn approve<'a>(
        &'a self,
        request: &'a PermissionRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        let approved = request.tool == QUALITY_CHECK_TOOL
            && self.commands.contains(&normalised(&request.detail));
        Box::pin(async move { approved })
    }
}

/// Start an authorized plan through the real `localbench`, under the lock.
///
/// `reuse_baseline` takes up the plan's [`ResumeOffer`]; it is ignored when the
/// plan carries none.
///
/// # Errors
/// [`UpliftRefusal::Busy`] when another run holds the lock.
#[allow(clippy::too_many_arguments)] // one run's whole context, each part named
pub async fn run_planned(
    root: &Path,
    candidate: &CandidateLesson,
    plan: &UpliftPlan,
    engine: &PermissionEngine,
    interactivity: Interactivity,
    cancel: &CancelSignal,
    reuse_baseline: bool,
    downweight: bool,
) -> Result<UpliftOutcome, UpliftRefusal> {
    let approver = PreviewedCommands::of(plan);
    // With no per-turn limit an arm is bounded by the wall clock alone.
    let wall = Duration::from_secs(plan.ceilings.wall_secs);
    let work = plan
        .ceilings
        .worst_case_secs(plan.tasks)
        .map_or(wall, |worst| {
            Duration::from_secs(worst.saturating_div(2).saturating_add(60))
        });
    let bench = LocalBenchCli {
        program: plan.tools.localbench.clone(),
        solver: plan.tools.solver.clone(),
        engine,
        approver: &approver,
        interactivity,
        cancel: cancel.clone(),
        // One arm's own work, plus slack; the wall-clock ceiling is the bound
        // on the whole run.
        arm_timeout: work.min(wall),
        cwd: root.to_path_buf(),
    };
    run_planned_with(
        root,
        candidate,
        plan,
        &bench,
        cancel,
        reuse_baseline,
        downweight,
    )
    .await
}

/// [`run_planned`] against any bench: take the lock, sweep expired runs, and
/// run under the plan's ceilings.
///
/// # Errors
/// [`UpliftRefusal::Busy`] when another run holds the lock.
pub async fn run_planned_with(
    root: &Path,
    candidate: &CandidateLesson,
    plan: &UpliftPlan,
    bench: &dyn UpliftBench,
    cancel: &CancelSignal,
    reuse_baseline: bool,
    downweight: bool,
) -> Result<UpliftOutcome, UpliftRefusal> {
    let _lock = ReplayLock::acquire_named(root, UPLIFT_LOCK).map_err(|refusal| match refusal {
        ReplayRefusal::Busy(detail) => UpliftRefusal::Busy(detail),
        other => UpliftRefusal::Untrusted(other.to_string()),
    })?;
    let swept = sweep_runs(root);
    let control = RunControl {
        run_dir: Some(plan.run_dir.clone()),
        cancel: cancel.clone(),
        wall: Some(Duration::from_secs(plan.ceilings.wall_secs)),
        max_tokens: Some(plan.ceilings.max_tokens),
        reuse_baseline: plan
            .resume
            .as_ref()
            .filter(|_| reuse_baseline)
            .map(|offer| offer.baseline.clone()),
    };
    let mut outcome = run_uplift_controlled(
        root,
        candidate,
        &plan.projection,
        &plan.settings,
        bench,
        downweight,
        &control,
    )
    .await;
    if !swept.is_empty() {
        outcome.evidence.limitations.push(format!(
            "{} expired run directorie(s) were removed first",
            swept.len()
        ));
    }
    Ok(outcome)
}

/// A stopped run of this exact request whose baseline arm had finished.
fn find_resume(root: &Path, binding: &str, settings: &UpliftSettings) -> Option<ResumeOffer> {
    let mut offers: Vec<(i64, ResumeOffer)> = run_dirs(root)
        .into_iter()
        .filter_map(|run_dir| {
            let state = read_run_state(&run_dir)?;
            let same = state.binding == binding
                && state.model == settings.model
                && state.trials == settings.trials
                && state.timeout_secs == settings.timeout_secs;
            let baseline = run_dir.join("baseline.json");
            let baseline_finished = baseline.is_file()
                && (state.reasons.iter().any(|reason| reason == "PartialPair")
                    || (!state.is_terminal()
                        && matches!(state.stage.as_str(), "lessons" | "combining")));
            (same && baseline_finished).then(|| {
                let ended = if state.is_terminal() {
                    format!("ended {}", state.reasons.join(", "))
                } else {
                    format!("interrupted during `{}`", state.stage)
                };
                (
                    state.updated_at,
                    ResumeOffer {
                        run_dir,
                        baseline,
                        ended,
                    },
                )
            })
        })
        .collect();
    offers.sort_by_key(|(updated, _)| *updated);
    offers.pop().map(|(_, offer)| offer)
}

fn uplift_root(root: &Path) -> PathBuf {
    root.join(".localpilot").join(LAB_UPLIFT_DIR)
}

fn run_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(uplift_root(root))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_dir())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs
}

/// How a run stands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunStanding {
    /// It reached an end and recorded it.
    Ended,
    /// Its process is at work now.
    Running,
    /// It stopped without recording an end: killed, or the machine went down.
    Interrupted,
}

/// Every run kept under the project, oldest first, with how it stands.
#[must_use]
pub fn run_statuses(root: &Path) -> Vec<(PathBuf, RunState, RunStanding)> {
    let lock = root.join(".localpilot").join("lab").join(UPLIFT_LOCK);
    let lock_live = std::fs::metadata(&lock)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age <= LOCK_STALE);
    let mut runs: Vec<(PathBuf, RunState, RunStanding)> = run_dirs(root)
        .into_iter()
        .filter_map(|run_dir| {
            let state = read_run_state(&run_dir)?;
            let standing = if state.is_terminal() {
                RunStanding::Ended
            } else if lock_live {
                RunStanding::Running
            } else {
                RunStanding::Interrupted
            };
            Some((run_dir, state, standing))
        })
        .collect();
    runs.sort_by_key(|(_, state, _)| state.started_at);
    // Only the newest unfinished run can be the one holding the lock.
    let newest_open = runs.iter().rposition(|(_, state, _)| !state.is_terminal());
    for (index, (_, _, standing)) in runs.iter_mut().enumerate() {
        if *standing == RunStanding::Running && Some(index) != newest_open {
            *standing = RunStanding::Interrupted;
        }
    }
    runs
}

/// Remove run directories past the lab's retention, as LocalMind plans it:
/// only directories strictly inside the project's own uplift directory.
/// Returns the names removed.
pub fn sweep_runs(root: &Path) -> Vec<String> {
    let base = uplift_root(root);
    if !base.is_dir() || !resolves_inside(root, &base) {
        return Vec::new();
    }
    let Ok(base) = dunce::canonicalize(&base) else {
        return Vec::new();
    };
    let entries: Vec<localmind_store::SessionEntry> = std::fs::read_dir(&base)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|entry| {
                    let meta = entry.metadata().ok()?;
                    let is_link = std::fs::symlink_metadata(entry.path())
                        .map(|meta| meta.file_type().is_symlink())
                        .unwrap_or(true);
                    (meta.is_dir() && !is_link).then(|| {
                        // A run's age is that of its last recorded state.
                        let modified =
                            std::fs::metadata(entry.path().join(crate::uplift_lab::RUN_STATE_FILE))
                                .and_then(|meta| meta.modified())
                                .or_else(|_| meta.modified())
                                .unwrap_or_else(|_| SystemTime::now());
                        localmind_store::SessionEntry {
                            id: entry.file_name().to_string_lossy().into_owned(),
                            path: entry.path(),
                            bytes: 0,
                            modified,
                        }
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    localmind_store::plan_lab_sweep(&base, &entries, SystemTime::now())
        .retention
        .prunable
        .into_iter()
        .filter(|entry| std::fs::remove_dir_all(&entry.path).is_ok())
        .map(|entry| entry.id)
        .collect()
}
