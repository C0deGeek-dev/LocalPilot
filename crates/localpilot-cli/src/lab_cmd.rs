//! `localpilot lab`: the lesson lab's explicit tiers.
//!
//! Logic runs by itself when a harness run completes. Replay does not: it runs
//! the project's own ratified check on real commits, so it is off unless the
//! project's committed `.localpilot.toml` enables it, and every run is shown
//! first and needs its own confirmation.

use std::io::{BufRead, Write};
use std::path::Path;
use std::time::Duration;

use localmind_core::AssignmentSource;
use localpilot_harness::{CancelSignal, QUALITY_CHECK_TOOL};

/// The permission engine Replay runs under: the configured profile, with the
/// quality-check identity allowlisted as it is for the ratified gate.
///
/// # Errors
/// Loading configuration.
pub fn engine(root: &Path) -> anyhow::Result<PermissionEngine> {
    let config = localpilot_config::load(
        &localpilot_config::ConfigPaths::standard(root),
        &localpilot_config::CliOverrides::default(),
    )?;
    Ok(PermissionEngine::new(
        crate::session_cmd::resolve_profile_from_config(&config),
        vec![QUALITY_CHECK_TOOL.to_string()],
    ))
}
use localpilot_localmind::{
    attach_lab_evidence, lab_candidate, plan_replay, read_lab_records, replay_preview, run_replay,
    ReplayPlan,
};
use localpilot_sandbox::{Interactivity, PermissionEngine};
use localpilot_store::Store;

/// How a Replay run is confirmed.
pub enum Confirmation<'a> {
    /// `--yes`: confirmed up front, headless.
    Yes,
    /// Ask on this terminal.
    Prompt(&'a mut dyn BufRead),
    /// No terminal and no `--yes`: nothing may run.
    Unavailable,
}

/// Every lesson the lab classified, what it can run, and the results each
/// lesson carries in review.
///
/// # Errors
/// Writing the output, or reading the review queue.
pub fn list(root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let store = Store::open(root);
    let records = read_lab_records(store.root());
    if records.is_empty() {
        writeln!(out, "No lessons have been classified for the lab yet.")?;
        return Ok(());
    }
    for record in &records {
        let sources: Vec<&str> = record
            .assignments
            .iter()
            .map(|assignment| source_name(assignment.source.as_ref()))
            .collect();
        let detail = if sources.is_empty() {
            record
                .reasons
                .iter()
                .map(|reason| format!("{reason:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            sources.join(", ")
        };
        writeln!(
            out,
            "{}  {:?} — {detail}",
            record.candidate_identity, record.eligibility
        )?;
        match lab_candidate(root, &record.candidate_identity)? {
            Some((_, candidate)) if !candidate.experiments.is_empty() => {
                for result in &candidate.experiments {
                    let stale = if result.is_stale_for(&candidate) {
                        " (stale)"
                    } else {
                        ""
                    };
                    writeln!(
                        out,
                        "    {:?} {:?}{}{stale}",
                        result.tier,
                        result.verdict,
                        reasons_suffix(&result.reasons)
                    )?;
                }
            }
            Some(_) => writeln!(out, "    no results yet")?,
            None => writeln!(out, "    no longer in review")?,
        }
        if let Some(state) =
            localpilot_localmind::lab_lesson_state(root, &record.candidate_identity)?
        {
            if !localpilot_localmind::lab_lesson_is_live(&state) {
                writeln!(
                    out,
                    "    history ({state:?}): these results describe a lesson that is no longer live"
                )?;
            }
        }
        for request in
            localpilot_localmind::rerun_requests(store.root(), &record.candidate_identity)
        {
            writeln!(
                out,
                "    rerun requested: {:?} by {}{} — not run; start it with `localpilot lab {}`",
                request.tier,
                request.requested_by,
                request
                    .note
                    .as_ref()
                    .map(|note| format!(" ({note})"))
                    .unwrap_or_default(),
                format!("{:?}", request.tier).to_lowercase()
            )?;
        }
    }
    Ok(())
}

/// Whether a run got as far as a result about the lesson. A run that was
/// refused, cancelled or could not execute leaves a rerun request open: the
/// thing that was asked for has not happened yet.
fn ran_to_a_result(verdict: localmind_core::LabVerdict) -> bool {
    !matches!(
        verdict,
        localmind_core::LabVerdict::InvalidExperiment | localmind_core::LabVerdict::NotExecutable
    )
}

/// The message for a lesson the lab no longer runs.
fn history_message(identity: &str, state: &localmind_core::ReviewState) -> String {
    format!(
        "{identity} is history ({state:?}): it was decided or replaced in review, and the lab \
         runs only live lessons"
    )
}

/// Record, or withdraw, a request that a lesson's Replay or Uplift run be done
/// again. Nothing runs.
///
/// # Errors
/// The lesson cannot be resolved, or the request is refused.
pub fn rerun(
    root: &Path,
    selection: &str,
    tier: &str,
    reviewer: &str,
    note: Option<String>,
    withdraw: bool,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let tier = match tier.trim().to_ascii_lowercase().as_str() {
        "replay" => localmind_core::EvidenceTier::Replay,
        "uplift" => localmind_core::EvidenceTier::Uplift,
        other => anyhow::bail!("`{other}` is not a run a person starts; use `replay` or `uplift`"),
    };
    let store = Store::open(root);
    let mut matches: Vec<_> = read_lab_records(store.root())
        .into_iter()
        .filter(|record| record.candidate_identity.starts_with(selection))
        .collect();
    let record = match matches.len() {
        0 => {
            anyhow::bail!("no classified lesson matches `{selection}` (see `localpilot lab list`)")
        }
        1 => matches.remove(0),
        n => anyhow::bail!("`{selection}` matches {n} lessons; give more of the identity"),
    };
    let identity = &record.candidate_identity;
    if withdraw {
        if localpilot_localmind::clear_rerun(store.root(), identity, tier) {
            writeln!(out, "Withdrew the {tier:?} rerun request for {identity}.")?;
        } else {
            writeln!(out, "No {tier:?} rerun request for {identity}.")?;
        }
        return Ok(());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        });
    let request = localpilot_localmind::request_rerun(
        root,
        store.root(),
        identity,
        tier,
        reviewer,
        note,
        now,
    )
    .map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
    writeln!(
        out,
        "Recorded: {} asks for {:?} to be run again for {identity}.",
        request.requested_by, request.tier
    )?;
    writeln!(
        out,
        "Nothing ran. A request does not enable {:?} for this project and is not a confirmation: \
         start it with `localpilot lab {} {identity}`, which shows what will run and asks.",
        request.tier,
        format!("{:?}", request.tier).to_lowercase()
    )?;
    Ok(())
}

/// Plan every Replay assignment for the lessons matching `selection` (a
/// candidate identity or a prefix of one; all when `None`), show what would
/// run, and run it only once confirmed. Each result goes onto its lesson in
/// review; an assignment that no longer holds is recorded as `Invalid` without
/// running anything.
///
/// # Errors
/// Writing the output, loading configuration, or reading the review queue.
#[allow(clippy::too_many_arguments)] // one command's whole context, each part named
pub async fn replay(
    root: &Path,
    selection: Option<&str>,
    confirmation: Confirmation<'_>,
    timeout: Duration,
    engine: &PermissionEngine,
    cancel: &CancelSignal,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let store = Store::open(root);
    let revision = git_head(root);
    let mut plans: Vec<ReplayPlan> = Vec::new();
    for record in read_lab_records(store.root()) {
        if selection.is_some_and(|wanted| !record.candidate_identity.starts_with(wanted)) {
            continue;
        }
        let replayable: Vec<_> = record
            .assignments
            .iter()
            .filter(|assignment| {
                matches!(
                    assignment.source,
                    Some(
                        AssignmentSource::FailFixPair { .. }
                            | AssignmentSource::ControlledMutation { .. }
                    )
                )
            })
            .collect();
        if replayable.is_empty() {
            continue;
        }
        let Some((_, candidate)) = lab_candidate(root, &record.candidate_identity)? else {
            writeln!(
                out,
                "{}: no longer in review; skipped",
                record.candidate_identity
            )?;
            continue;
        };
        if let Some(state) =
            localpilot_localmind::lab_lesson_state(root, &record.candidate_identity)?
        {
            if !localpilot_localmind::lab_lesson_is_live(&state) {
                writeln!(
                    out,
                    "{}; skipped",
                    history_message(&record.candidate_identity, &state)
                )?;
                continue;
            }
        }
        for assignment in replayable {
            match plan_replay(root, &candidate, assignment, timeout) {
                Ok(plan) => plans.push(plan),
                Err(refusal) => {
                    writeln!(out, "{}: {refusal}", record.candidate_identity)?;
                    if let Some(evidence) = refusal.evidence(&candidate, assignment, &revision) {
                        attach_lab_evidence(root, evidence)?;
                        writeln!(out, "  recorded as Invalid on the lesson in review")?;
                    }
                }
            }
        }
    }
    if plans.is_empty() {
        writeln!(out, "Nothing to replay.")?;
        return Ok(());
    }
    for plan in &plans {
        write!(out, "{}", replay_preview(plan))?;
    }

    let interactivity = match confirmation {
        Confirmation::Yes => Interactivity::NonInteractive,
        Confirmation::Unavailable => {
            writeln!(
                out,
                "Nothing ran: confirm on a terminal, or pass --yes to run without asking."
            )?;
            return Ok(());
        }
        Confirmation::Prompt(input) => {
            write!(out, "Run {} Replay run(s)? [y/N] ", plans.len())?;
            out.flush()?;
            let mut answer = String::new();
            input.read_line(&mut answer)?;
            if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                writeln!(out, "Nothing ran.")?;
                return Ok(());
            }
            Interactivity::Interactive
        }
    };

    for plan in &plans {
        if cancel.is_cancelled() {
            writeln!(out, "Cancelled; nothing further ran.")?;
            break;
        }
        let outcome = match run_replay(root, plan, engine, interactivity, cancel).await {
            Ok(outcome) => outcome,
            Err(refusal) => {
                writeln!(out, "Nothing ran: {refusal}")?;
                break;
            }
        };
        writeln!(
            out,
            "{}: Replay {:?}{}",
            plan.candidate_identity,
            outcome.evidence.verdict,
            reasons_suffix(&outcome.evidence.reasons)
        )?;
        for arm in &outcome.receipt.arms {
            writeln!(out, "  {}: {} ({})", arm.name, arm.end, arm.cleanup)?;
        }
        for swept in &outcome.receipt.swept_worktrees {
            writeln!(out, "  swept a leftover worktree: {swept}")?;
        }
        if let Some(path) = &outcome.receipt_path {
            writeln!(out, "  receipt: {}", path.display())?;
        }
        let ran = ran_to_a_result(outcome.evidence.verdict);
        if !attach_lab_evidence(root, outcome.evidence)? {
            writeln!(
                out,
                "  the lesson is no longer in review; the result was not kept"
            )?;
        }
        if ran {
            localpilot_localmind::clear_rerun(
                store.root(),
                &plan.candidate_identity,
                localmind_core::EvidenceTier::Replay,
            );
        }
    }
    Ok(())
}

/// The one classified lesson `selection` names: a candidate identity or an
/// unambiguous prefix of one, still in review.
fn resolve(
    root: &Path,
    selection: &str,
) -> anyhow::Result<(
    localpilot_localmind::LabClassification,
    localmind_core::CandidateLesson,
)> {
    let store = Store::open(root);
    let mut matches: Vec<_> = read_lab_records(store.root())
        .into_iter()
        .filter(|record| record.candidate_identity.starts_with(selection))
        .collect();
    let record = match matches.len() {
        0 => {
            anyhow::bail!("no classified lesson matches `{selection}` (see `localpilot lab list`)")
        }
        1 => matches.remove(0),
        n => anyhow::bail!("`{selection}` matches {n} lessons; give more of the identity"),
    };
    let Some((_, candidate)) = lab_candidate(root, &record.candidate_identity)? else {
        anyhow::bail!("{} is no longer in review", record.candidate_identity);
    };
    if let Some(state) = localpilot_localmind::lab_lesson_state(root, &record.candidate_identity)? {
        if !localpilot_localmind::lab_lesson_is_live(&state) {
            anyhow::bail!("{}", history_message(&record.candidate_identity, &state));
        }
    }
    Ok((record, candidate))
}

fn print_tasks(set: &localpilot_localmind::LabTaskSet, out: &mut dyn Write) -> std::io::Result<()> {
    for task in &set.tasks {
        writeln!(out, "  {}: {}", task.id, task.prompt)?;
        writeln!(out, "      expects: {}", task.expect)?;
    }
    Ok(())
}

/// Have the configured model draft uplift tasks for a lesson, and write them as
/// a draft for a person to read, edit and approve. A draft tests nothing.
///
/// Sends the lesson and its hindsight — not the run's raw facts — to the
/// provider the project is already configured to use.
///
/// # Errors
/// The lesson cannot be resolved, the model cannot draft, or the draft cannot
/// be written.
pub async fn tasks_draft(
    root: &Path,
    selection: &str,
    model: &str,
    provider: &dyn localpilot_llm::ModelProvider,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let (record, candidate) = resolve(root, selection)?;
    let drafted = localpilot_localmind::draft_tasks(provider, model, &candidate)
        .await
        .map_err(|failure| anyhow::anyhow!("no draft: {failure}"))?;
    let store = Store::open(root);
    let path = localpilot_localmind::write_draft(store.root(), &drafted.set)?;
    writeln!(
        out,
        "Drafted {} task(s) for {} with {model} ({} model call(s){}):",
        drafted.set.tasks.len(),
        record.candidate_identity,
        drafted.model_calls,
        if drafted.repaired { ", one repair" } else { "" }
    )?;
    print_tasks(&drafted.set, out)?;
    writeln!(out, "Draft: {}", path.display())?;
    writeln!(
        out,
        "A draft tests nothing. Read it — would a model get these wrong without the lesson, and \
         is the expected text the behaviour the lesson is about? Edit the file if needed, then:"
    )?;
    writeln!(
        out,
        "  localpilot lab tasks approve {} --reviewer <your name>",
        record.candidate_identity
    )?;
    Ok(())
}

/// Show a lesson's draft and its approved task set, if any.
///
/// # Errors
/// The lesson cannot be resolved or a file cannot be read.
pub fn tasks_show(root: &Path, selection: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    let (record, candidate) = resolve(root, selection)?;
    let store = Store::open(root);
    writeln!(
        out,
        "{}: {}",
        record.candidate_identity,
        candidate.summary()
    )?;
    let draft = localpilot_localmind::read_task_set(&localpilot_localmind::draft_path(
        store.root(),
        &record.candidate_identity,
    ))
    .map_err(anyhow::Error::msg)?;
    match &draft {
        Some(set) => {
            writeln!(
                out,
                "Draft ({} task(s){}):",
                set.tasks.len(),
                set.drafted_by
                    .as_ref()
                    .map(|model| format!(", drafted by {model}"))
                    .unwrap_or_default()
            )?;
            print_tasks(set, out)?;
            if let Err(problems) = localpilot_localmind::validate_tasks(set, &candidate) {
                for problem in problems {
                    writeln!(out, "  cannot be approved: {problem}")?;
                }
            }
        }
        None => writeln!(out, "No draft.")?,
    }
    match localpilot_localmind::approved_tasks(store.root(), &candidate) {
        Ok(Some(set)) => {
            writeln!(
                out,
                "Approved by {} ({} task(s), {}):",
                set.approved_by.as_deref().unwrap_or("?"),
                set.tasks.len(),
                set.content_hash()
            )?;
            print_tasks(&set, out)?;
        }
        Ok(None) => writeln!(out, "Not approved.")?,
        Err(problem) => writeln!(out, "The approved set no longer holds: {problem}")?,
    }
    Ok(())
}

/// Approve a lesson's draft in `reviewer`'s name: freeze it as the lesson's
/// uplift assignment and keep it in the lesson's lab record.
///
/// # Errors
/// The lesson cannot be resolved, the approval is refused, or the record
/// cannot be written.
pub fn tasks_approve(
    root: &Path,
    selection: &str,
    reviewer: &str,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let (mut record, candidate) = resolve(root, selection)?;
    let store = Store::open(root);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        });
    let (set, assignment) =
        localpilot_localmind::approve_tasks(store.root(), &candidate, reviewer, now)
            .map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
    // One approved task set per lesson: a new approval replaces the old one.
    record.assignments.retain(|assignment| {
        !matches!(
            assignment.source,
            Some(AssignmentSource::ApprovedTaskSet { .. })
        )
    });
    record.assignments.push(assignment);
    localpilot_localmind::write_lab_record(store.root(), &record)?;
    writeln!(
        out,
        "Approved {} task(s) for {} as {}. Frozen as {}.",
        set.tasks.len(),
        record.candidate_identity,
        set.approved_by.as_deref().unwrap_or_default(),
        set.content_hash()
    )?;
    Ok(())
}

/// What `lab uplift` was asked for.
#[derive(Debug, Clone, Default)]
pub struct UpliftArgs {
    pub model: String,
    pub trials: Option<u32>,
    pub turn_timeout_secs: Option<u64>,
    pub wall_minutes: Option<u64>,
    pub max_tokens: Option<u64>,
    /// Reuse a finished baseline an earlier, stopped run of this request left.
    pub resume: bool,
    /// Run both arms even though such a baseline exists.
    pub restart: bool,
}

/// The programs an uplift run starts: `localbench` from the user's own
/// configuration (never the project's), and this `localpilot` as the solver.
///
/// # Errors
/// The user configuration cannot be loaded, or this program's path is unknown.
pub fn uplift_tools() -> anyhow::Result<localpilot_localmind::UpliftTools> {
    let paths = localpilot_config::ConfigPaths {
        user: localpilot_config::user_config_path(),
        project: None,
    };
    let config = localpilot_config::load(&paths, &localpilot_config::CliOverrides::default())?;
    let solver = std::env::current_exe()?;
    Ok(localpilot_localmind::UpliftTools {
        localbench: config.lab.localbench,
        solver: solver.to_string_lossy().into_owned(),
    })
}

/// Plan an uplift run for a lesson, show what it will do and what bounds it,
/// and run it only once confirmed. The result goes onto the lesson in review.
///
/// `bench` stands in for LocalBench in tests; `None` runs the real program
/// through the permission engine.
///
/// # Errors
/// The lesson cannot be resolved, or output, configuration or the review queue
/// fails.
#[allow(clippy::too_many_arguments)] // one command's whole context, each part named
pub async fn uplift(
    root: &Path,
    selection: &str,
    args: &UpliftArgs,
    confirmation: Confirmation<'_>,
    engine: &PermissionEngine,
    tools: localpilot_localmind::UpliftTools,
    bench: Option<&dyn localpilot_localmind::UpliftBench>,
    cancel: &CancelSignal,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    use localpilot_localmind::{UpliftCeilings, UpliftRefusal};

    let (record, candidate) = resolve(root, selection)?;
    let Some(assignment) = record.assignments.iter().find(|assignment| {
        matches!(
            assignment.source,
            Some(AssignmentSource::ApprovedTaskSet { .. })
        )
    }) else {
        writeln!(out, "Nothing ran: {}", UpliftRefusal::NoApprovedTasks)?;
        return Ok(());
    };
    let defaults = UpliftCeilings::default();
    let ceilings = UpliftCeilings {
        trials: args.trials.unwrap_or(defaults.trials),
        turn_timeout_secs: args.turn_timeout_secs.unwrap_or(defaults.turn_timeout_secs),
        wall_secs: args
            .wall_minutes
            .map_or(defaults.wall_secs, |minutes| minutes.saturating_mul(60)),
        max_tokens: args.max_tokens.unwrap_or(defaults.max_tokens),
    };
    let plan = match localpilot_localmind::plan_uplift(
        root,
        &candidate,
        assignment,
        &args.model,
        ceilings,
        tools,
    ) {
        Ok(plan) => plan,
        Err(refusal) => {
            writeln!(out, "Nothing ran: {refusal}")?;
            return Ok(());
        }
    };
    write!(out, "{}", localpilot_localmind::uplift_authorization(&plan))?;

    let offered = plan.resume.is_some();
    if args.resume && args.restart {
        writeln!(out, "Nothing ran: pass --resume or --restart, not both.")?;
        return Ok(());
    }
    let (interactivity, reuse) = match confirmation {
        Confirmation::Unavailable => {
            writeln!(
                out,
                "Nothing ran: confirm on a terminal, or pass --yes to run without asking."
            )?;
            return Ok(());
        }
        Confirmation::Yes => {
            // Headless: an existing baseline is never reused or repeated on a
            // guess.
            if offered && !args.resume && !args.restart {
                writeln!(
                    out,
                    "Nothing ran: an earlier baseline exists. Pass --resume to reuse it or \
                     --restart to run both arms again."
                )?;
                return Ok(());
            }
            (Interactivity::NonInteractive, offered && args.resume)
        }
        Confirmation::Prompt(input) => {
            let mut ask = |question: &str, out: &mut dyn Write| -> anyhow::Result<bool> {
                write!(out, "{question} [y/N] ")?;
                out.flush()?;
                let mut answer = String::new();
                input.read_line(&mut answer)?;
                Ok(matches!(
                    answer.trim().to_ascii_lowercase().as_str(),
                    "y" | "yes"
                ))
            };
            if !ask("Run this uplift run?", out)? {
                writeln!(out, "Nothing ran.")?;
                return Ok(());
            }
            let reuse = if !offered || args.restart {
                false
            } else if args.resume {
                true
            } else {
                ask(
                    "Reuse the finished baseline instead of running it again?",
                    out,
                )?
            };
            (Interactivity::Interactive, reuse)
        }
    };

    let config = localpilot_config::load(
        &localpilot_config::ConfigPaths::standard(root),
        &localpilot_config::CliOverrides::default(),
    )?;
    let downweight = config.memory.outcome_downweight;
    let ran = match bench {
        Some(bench) => {
            localpilot_localmind::run_planned_with(
                root, &candidate, &plan, bench, cancel, reuse, downweight,
            )
            .await
        }
        None => {
            localpilot_localmind::run_planned(
                root,
                &candidate,
                &plan,
                engine,
                interactivity,
                cancel,
                reuse,
                downweight,
            )
            .await
        }
    };
    let outcome = match ran {
        Ok(outcome) => outcome,
        Err(refusal) => {
            writeln!(out, "Nothing ran: {refusal}")?;
            return Ok(());
        }
    };
    writeln!(
        out,
        "{}: Uplift {:?}{}",
        record.candidate_identity,
        outcome.evidence.verdict,
        reasons_suffix(&outcome.evidence.reasons)
    )?;
    for limitation in outcome.evidence.limitations.iter().skip(1) {
        writeln!(out, "  {limitation}")?;
    }
    let denied = outcome.evidence.reasons.iter().any(
        |reason| matches!(reason, localmind_core::VerdictReason::Other(name) if name == localpilot_localmind::PERMISSION_DENIED),
    );
    if denied && interactivity == Interactivity::NonInteractive {
        writeln!(
            out,
            "  `--yes` runs headless, and a headless run cannot answer the permission question \
             for this command. Confirm on a terminal instead, or use a permission profile that \
             allows it without asking."
        )?;
    }
    print_telemetry(&outcome.telemetry, out)?;
    for id in &outcome.flagged_for_review {
        writeln!(out, "  routed to review: accepted memory {id}")?;
    }
    writeln!(out, "  run: {}", outcome.run_dir.display())?;
    let ran = ran_to_a_result(outcome.evidence.verdict);
    if !attach_lab_evidence(root, outcome.evidence)? {
        writeln!(
            out,
            "  the lesson is no longer in review; the result was not kept"
        )?;
    }
    if ran {
        localpilot_localmind::clear_rerun(
            Store::open(root).root(),
            &record.candidate_identity,
            localmind_core::EvidenceTier::Uplift,
        );
    }
    Ok(())
}

fn print_telemetry(
    telemetry: &localpilot_localmind::Telemetry,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let seconds = |millis: Option<u64>| {
        millis.map_or_else(
            || "not run".to_string(),
            |millis| format!("{:.1} s", millis as f64 / 1000.0),
        )
    };
    let or_unmeasured =
        |value: &Option<String>| value.clone().unwrap_or_else(|| "not measured".to_string());
    writeln!(
        out,
        "  wall time: {:.1} s (baseline {}, lesson arm {})",
        telemetry.total_wall_ms as f64 / 1000.0,
        seconds(telemetry.baseline_wall_ms),
        seconds(telemetry.lessons_wall_ms)
    )?;
    writeln!(
        out,
        "  tokens: {}",
        telemetry.tokens.map_or_else(
            || "not reported by the sessions".to_string(),
            |t| t.to_string()
        )
    )?;
    writeln!(
        out,
        "  model load state: {}; RAM: {}; GPU: {}",
        or_unmeasured(&telemetry.model_state),
        or_unmeasured(&telemetry.ram),
        or_unmeasured(&telemetry.gpu)
    )
}

/// Show every uplift run kept for the project and how it stands.
///
/// # Errors
/// Writing the output.
pub fn status(root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    use localpilot_localmind::RunStanding;
    let runs = localpilot_localmind::run_statuses(root);
    if runs.is_empty() {
        writeln!(out, "No uplift runs.")?;
        return Ok(());
    }
    for (dir, state, standing) in runs {
        let standing = match standing {
            RunStanding::Running => format!("running ({})", state.stage),
            RunStanding::Interrupted => format!(
                "interrupted during `{}` — not a result; run it again",
                state.stage
            ),
            RunStanding::Ended => format!(
                "{}{}",
                state.verdict.clone().unwrap_or_default(),
                if state.reasons.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", state.reasons.join(", "))
                }
            ),
        };
        writeln!(
            out,
            "{}  {}  model {}  {standing}",
            dir.file_name().unwrap_or_default().to_string_lossy(),
            state.candidate_identity,
            state.model
        )?;
    }
    Ok(())
}

fn source_name(source: Option<&AssignmentSource>) -> &'static str {
    match source {
        Some(AssignmentSource::RecordedTrajectory { .. }) => "recorded trajectory",
        Some(AssignmentSource::FailFixPair { .. }) => "fail/fix pair",
        Some(AssignmentSource::ControlledMutation { .. }) => "controlled mutation",
        Some(AssignmentSource::RatifiedCheck { .. }) => "ratified check",
        Some(AssignmentSource::ApprovedTaskSet { .. }) => "approved task set",
        _ => "other",
    }
}

fn reasons_suffix(reasons: &[localmind_core::VerdictReason]) -> String {
    if reasons.is_empty() {
        String::new()
    } else {
        format!(
            " — {}",
            reasons
                .iter()
                .map(|reason| format!("{reason:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

fn git_head(root: &Path) -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map_or_else(
            || "unknown".to_string(),
            |output| String::from_utf8_lossy(&output.stdout).trim().to_string(),
        )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use localmind_core::{
        CandidateLesson, CausalHypothesis, Confidence, EvidenceKind, EvidenceRef, HindsightDraft,
        LabVerdict, LessonCategory, LessonId, Observation, SuggestedAction, VerdictReason,
    };
    use localpilot_config::CheckConfig;
    use localpilot_harness::{check_command_digest, Progress};
    use localpilot_localmind::{classify_for_lab, LabContext, RATIFIED_CHECK_KEY};
    use localpilot_sandbox::Profile;

    const SESSION: &str = "0b0e6c1e-0000-4000-8000-000000000009";

    fn git(root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
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

    #[cfg(windows)]
    fn check() -> CheckConfig {
        check_of("findstr", &["/b", "/c:fixed", "state.txt"])
    }
    #[cfg(not(windows))]
    fn check() -> CheckConfig {
        check_of("grep", &["-qx", "fixed", "state.txt"])
    }

    fn check_of(program: &str, args: &[&str]) -> CheckConfig {
        CheckConfig {
            name: "test".to_string(),
            program: program.to_string(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
            fix_program: None,
            fix_args: Vec::new(),
            cadence: localpilot_config::Cadence::default(),
            auto_fix: localpilot_config::AutoFix::default(),
            severity: None,
        }
    }

    fn config_text(check: &CheckConfig) -> String {
        format!(
            "[lab]\nreplay = true\nuplift = true\n\n[[harness.checks]]\nname = \"test\"\nprogram = {:?}\nargs = {:?}\n",
            check.program, check.args
        )
    }

    /// A project with a broken base, a fixing step, its lesson queued in
    /// review, and the lesson's frozen lab record — what a completed run
    /// leaves behind.
    fn project() -> (tempfile::TempDir, CandidateLesson) {
        let dir = tempfile::tempdir().unwrap();
        let candidate = project_at(
            dir.path(),
            "Write the state file before the check that reads it",
            "the state file had not been written yet",
        );
        (dir, candidate)
    }

    fn project_at(root: &Path, lesson: &str, claim: &str) -> CandidateLesson {
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "test@example.com"]);
        git(root, &["config", "user.name", "Test"]);
        std::fs::write(
            root.join(".localmind.toml"),
            "[learning]\nenabled = true\nallowed_scopes = [\"project\"]\n",
        )
        .unwrap();
        let check = check();
        commit(
            root,
            &[
                (".localpilot.toml", &config_text(&check)),
                (".gitignore", ".localpilot/\n.localmind/\n.localmind.toml\n"),
                ("state.txt", "broken\n"),
            ],
            "base",
        );
        let fix = commit(
            root,
            &[("state.txt", "fixed\n")],
            "harness: write the state",
        );

        let mut failed = EvidenceRef::identified(
            EvidenceKind::TestOutput,
            "ratified check `test` failed (step)",
            format!("localpilot-session:{SESSION}"),
            format!("localpilot-session:{SESSION}#event:c1"),
            "sha256:c1",
        )
        .redacted()
        .with_observation(Observation::Failure)
        .with_signature(format!("check:test:{}", check_command_digest(&check)));
        failed
            .metadata
            .insert(RATIFIED_CHECK_KEY.to_string(), "test".to_string());
        let mut draft = HindsightDraft::new("Write the state", "The check passes").with_hypothesis(
            CausalHypothesis {
                claim: claim.to_string(),
                evidence_ids: vec![failed.id.clone()],
                confidence: Confidence::new(0.6).unwrap(),
            },
        );
        draft.proposed_lesson = Some(lesson.to_string());
        let candidate = CandidateLesson::new(
            LessonId::new("retro-1"),
            lesson,
            LessonCategory::Process,
            Confidence::new(0.4).unwrap(),
            SuggestedAction::PromoteToMemory,
        )
        .with_evidence(failed)
        .with_hindsight(draft);

        localmind_store::ReviewQueue::open_project(root)
            .unwrap()
            .enqueue_candidates(
                &localmind_core::SessionId::new("completion-retrospective"),
                std::slice::from_ref(&candidate),
            )
            .unwrap();
        let progress = Progress::parse(&format!(
            "# Progress: state\nBranch: feature/state\n\n## Steps\n\n\
- [x] 1. Write the state\n  - commit: {}\n  - attempts: 1\n  - sessions: {SESSION}\n",
            &fix[..7]
        ))
        .unwrap();
        let record = classify_for_lab(
            &candidate,
            &LabContext {
                root,
                progress: Some(&progress),
                checks: std::slice::from_ref(&check),
            },
        );
        assert!(!record.assignments.is_empty(), "{record:?}");
        std::fs::create_dir_all(root.join(".localpilot").join("lab").join("assignments")).unwrap();
        std::fs::write(
            root.join(".localpilot")
                .join("lab")
                .join("assignments")
                .join(format!("{}.json", record.candidate_identity)),
            serde_json::to_string_pretty(&record).unwrap(),
        )
        .unwrap();
        candidate
    }

    /// Lay down the project a live uplift run starts from: a lesson in review
    /// that states a convention no model could know, so only the lesson arm
    /// can answer. Drive it with the real commands afterwards:
    /// `lab tasks draft`, `lab tasks approve`, `lab uplift`.
    ///
    /// Run with:
    ///   `LOCALPILOT_LIVE_TESTS=1 LOCALPILOT_LIVE_UPLIFT_DIR=<empty dir> cargo test -p localpilot a_live_uplift_project -- --nocapture`
    #[test]
    fn a_live_uplift_project_is_laid_down_on_request() {
        let dir = std::env::var_os("LOCALPILOT_LIVE_UPLIFT_DIR");
        let (Ok(_), Some(dir)) = (std::env::var("LOCALPILOT_LIVE_TESTS"), dir) else {
            eprintln!("skipping the live uplift project: set LOCALPILOT_LIVE_TESTS and LOCALPILOT_LIVE_UPLIFT_DIR");
            return;
        };
        let root = Path::new(&dir);
        std::fs::create_dir_all(root).unwrap();
        let candidate = project_at(
            root,
            "In this project database migrations are applied only with `zorp migrate --apply-now`; no other command applies them",
            "the migrations had not been applied, because the usual migrate command does nothing here",
        );
        println!("lesson {}", candidate.content_identity());
    }

    /// The live run itself, confirmed at the prompt as a person would, in the
    /// project laid down above once its tasks are approved. The solver is a
    /// real `localpilot` talking to whatever provider its environment names.
    ///
    /// Run with `LOCALPILOT_LIVE_TESTS=1`, `LOCALPILOT_LIVE_UPLIFT_DIR`,
    /// `LOCALPILOT_LIVE_MODEL`, `LOCALPILOT_TEST_LOCALBENCH` and
    /// `LOCALPILOT_LIVE_SOLVER` (a `localpilot` program) set, `--nocapture`.
    #[tokio::test]
    async fn a_live_uplift_run_is_confirmed_at_the_prompt_on_request() {
        let var = |name: &str| std::env::var(name).ok();
        let (Some(_), Some(dir), Some(model), Some(localbench), Some(solver)) = (
            var("LOCALPILOT_LIVE_TESTS"),
            var("LOCALPILOT_LIVE_UPLIFT_DIR"),
            var("LOCALPILOT_LIVE_MODEL"),
            var("LOCALPILOT_TEST_LOCALBENCH"),
            var("LOCALPILOT_LIVE_SOLVER"),
        ) else {
            eprintln!("skipping the live uplift run: its environment is not set");
            return;
        };
        let args = UpliftArgs {
            model,
            // A local model can need minutes per turn: one trial, a long turn.
            trials: Some(1),
            turn_timeout_secs: Some(300),
            wall_minutes: Some(25),
            ..UpliftArgs::default()
        };
        let mut yes = std::io::Cursor::new(b"y\n".to_vec());
        let mut out = Vec::new();
        uplift(
            Path::new(&dir),
            "cnd-",
            &args,
            Confirmation::Prompt(&mut yes),
            &engine(),
            localpilot_localmind::UpliftTools { localbench, solver },
            None,
            &CancelSignal::new(),
            &mut out,
        )
        .await
        .unwrap();
        println!("{}", String::from_utf8(out).unwrap());
    }

    fn engine() -> PermissionEngine {
        PermissionEngine::new(Profile::Default, vec![QUALITY_CHECK_TOOL.to_string()])
    }

    async fn run(root: &Path, confirmation: Confirmation<'_>) -> String {
        let mut out = Vec::new();
        replay(
            root,
            None,
            confirmation,
            Duration::from_secs(60),
            &engine(),
            &CancelSignal::new(),
            &mut out,
        )
        .await
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    fn results(root: &Path, candidate: &CandidateLesson) -> Vec<(LabVerdict, Vec<VerdictReason>)> {
        let (_, stored) = lab_candidate(root, &candidate.content_identity())
            .unwrap()
            .expect("still in review");
        stored
            .experiments
            .iter()
            .map(|result| (result.verdict, result.reasons.clone()))
            .collect()
    }

    fn worktrees(root: &Path) -> usize {
        std::fs::read_dir(root.join(".localpilot").join("worktrees"))
            .map(|entries| entries.count())
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn yes_runs_the_previewed_replay_and_the_result_reaches_review() {
        let (dir, candidate) = project();
        let root = dir.path();

        let printed = run(root, Confirmation::Yes).await;

        assert!(
            printed.contains("not a sandbox"),
            "the preview comes first: {printed}"
        );
        assert!(printed.contains("Replay Valid"), "{printed}");
        assert!(
            printed.contains("expect-fail: exited 1 (removed)"),
            "{printed}"
        );
        assert!(printed.contains("receipt: "), "{printed}");
        assert_eq!(
            results(root, &candidate),
            vec![(LabVerdict::Valid, Vec::new())]
        );
        assert_eq!(worktrees(root), 0);

        let mut listing = Vec::new();
        list(root, &mut listing).unwrap();
        let listing = String::from_utf8(listing).unwrap();
        assert!(listing.contains("Replay — fail/fix pair"), "{listing}");
        assert!(listing.contains("Replay Valid"), "{listing}");

        // Run again: the same inputs and verdict are not stored twice.
        run(root, Confirmation::Yes).await;
        assert_eq!(results(root, &candidate).len(), 1);
    }

    #[tokio::test]
    async fn nothing_runs_without_a_confirmation() {
        let (dir, candidate) = project();
        let root = dir.path();

        let printed = run(root, Confirmation::Unavailable).await;
        assert!(printed.contains("Nothing ran"), "{printed}");

        let mut declined = std::io::Cursor::new(b"n\n".to_vec());
        let printed = run(root, Confirmation::Prompt(&mut declined)).await;
        assert!(
            printed.contains("[y/N]") && printed.contains("Nothing ran."),
            "{printed}"
        );

        assert!(results(root, &candidate).is_empty());
        assert_eq!(worktrees(root), 0);
        assert!(!root.join(".localpilot").join("lab").join("runs").exists());

        let mut accepted = std::io::Cursor::new(b"y\n".to_vec());
        let printed = run(root, Confirmation::Prompt(&mut accepted)).await;
        assert!(printed.contains("Replay Valid"), "{printed}");
    }

    #[tokio::test]
    async fn an_assignment_that_no_longer_holds_is_recorded_invalid_without_running() {
        let (dir, candidate) = project();
        let root = dir.path();
        let loosened = check_of("findstr", &["/c:e", "state.txt"]);
        commit(
            root,
            &[(".localpilot.toml", &config_text(&loosened))],
            "loosen",
        );

        let printed = run(root, Confirmation::Yes).await;

        assert!(printed.contains("no longer holds"), "{printed}");
        assert!(printed.contains("recorded as Invalid"), "{printed}");
        assert!(printed.contains("Nothing to replay."), "{printed}");
        assert_eq!(
            results(root, &candidate),
            vec![(LabVerdict::Invalid, vec![VerdictReason::OracleMutable])]
        );
        assert_eq!(worktrees(root), 0);
    }

    #[tokio::test]
    async fn a_drafted_task_set_runs_nothing_until_a_named_person_approves_it() {
        use localpilot_llm::FakeProvider;

        let (dir, candidate) = project();
        let root = dir.path();
        let identity = candidate.content_identity();
        let reply = r#"{"tasks":[
            {"prompt":"The check reads a file that is not there yet. What should happen first?","expect":"write the state file"},
            {"prompt":"In which order do the state file and its check go?","expect":"state file first"}
        ]}"#;
        let provider = FakeProvider::new().text(reply);

        // Draft, by an unambiguous prefix of the identity.
        let mut out = Vec::new();
        tasks_draft(root, &identity[..10], "local-model", &provider, &mut out)
            .await
            .unwrap();
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("Drafted 2 task(s)"), "{printed}");
        assert!(printed.contains("A draft tests nothing"), "{printed}");
        let store = Store::open(root);
        assert!(localpilot_localmind::draft_path(store.root(), &identity).is_file());

        let uplift = |root: &Path| {
            read_lab_records(Store::open(root).root())[0]
                .assignments
                .iter()
                .find(|assignment| {
                    matches!(
                        assignment.source,
                        Some(AssignmentSource::ApprovedTaskSet { .. })
                    )
                })
                .cloned()
        };
        assert_eq!(uplift(root), None, "a draft is not an assignment");
        let mut out = Vec::new();
        tasks_show(root, &identity, &mut out).unwrap();
        let shown = String::from_utf8(out).unwrap();
        assert!(shown.contains("drafted by local-model") && shown.contains("Not approved."));

        // An approval needs a name.
        let mut out = Vec::new();
        assert!(tasks_approve(root, &identity, " ", &mut out).is_err());
        assert_eq!(uplift(root), None);

        let mut out = Vec::new();
        tasks_approve(root, &identity, "reviewer", &mut out).unwrap();
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("Approved 2 task(s)") && printed.contains("as reviewer"));

        // The frozen assignment is in the lesson's lab record, beside its
        // Replay assignment, and projects into a run.
        let assignment = uplift(root).expect("the approval froze an assignment");
        assert_eq!(
            assignment.source,
            Some(AssignmentSource::ApprovedTaskSet {
                approved_by: "reviewer".to_string(),
                drafted_by: Some("local-model".to_string()),
            })
        );
        assert!(read_lab_records(store.root())[0].assignments.len() >= 2);
        let tasks = localpilot_localmind::approved_tasks(store.root(), &candidate)
            .unwrap()
            .unwrap();
        let projection =
            localpilot_localmind::project_uplift(&candidate, &assignment, &tasks, "rev").unwrap();
        assert_eq!(projection.lineage.candidate_identity, identity);

        let mut listing = Vec::new();
        list(root, &mut listing).unwrap();
        assert!(String::from_utf8(listing)
            .unwrap()
            .contains("approved task set"));

        // Approving again replaces the approved set rather than adding a second.
        let mut out = Vec::new();
        tasks_approve(root, &identity, "second reviewer", &mut out).unwrap();
        let approved: Vec<_> = read_lab_records(store.root())[0]
            .assignments
            .iter()
            .filter(|a| matches!(a.source, Some(AssignmentSource::ApprovedTaskSet { .. })))
            .cloned()
            .collect();
        assert_eq!(approved.len(), 1);

        let mut out = Vec::new();
        assert!(tasks_show(root, "cnd-does-not-exist", &mut out).is_err());
    }

    /// Give the fixture's lesson an approved task set, as `lab tasks` would.
    async fn approve_fixture_tasks(root: &Path, identity: &str) {
        use localpilot_llm::FakeProvider;
        let reply = r#"{"tasks":[
            {"prompt":"The check reads a file that is not there yet. What should happen first?","expect":"foo db sync"}
        ]}"#;
        let mut out = Vec::new();
        tasks_draft(
            root,
            identity,
            "local-model",
            &FakeProvider::new().text(reply),
            &mut out,
        )
        .await
        .unwrap();
        tasks_approve(root, identity, "reviewer", &mut out).unwrap();
    }

    fn uplift_args() -> UpliftArgs {
        UpliftArgs {
            model: "fixture-model".to_string(),
            ..UpliftArgs::default()
        }
    }

    fn fixture_tools() -> localpilot_localmind::UpliftTools {
        localpilot_localmind::UpliftTools {
            localbench: "localbench".to_string(),
            solver: "localpilot".to_string(),
        }
    }

    async fn run_uplift_command(
        root: &Path,
        identity: &str,
        confirmation: Confirmation<'_>,
        tools: localpilot_localmind::UpliftTools,
    ) -> String {
        let mut out = Vec::new();
        uplift(
            root,
            identity,
            &uplift_args(),
            confirmation,
            &engine(),
            tools,
            None,
            &CancelSignal::new(),
            &mut out,
        )
        .await
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    #[tokio::test]
    async fn an_uplift_run_starts_only_from_an_explicit_confirmed_command() {
        let (dir, candidate) = project();
        let root = dir.path();
        let identity = candidate.content_identity();

        // No approved tasks: nothing to run.
        let printed = run_uplift_command(root, &identity, Confirmation::Yes, fixture_tools()).await;
        assert!(printed.contains("Nothing ran") && printed.contains("no approved task set"));

        approve_fixture_tasks(root, &identity).await;

        // Shown, with its ceilings, but not confirmed: nothing runs.
        let printed =
            run_uplift_command(root, &identity, Confirmation::Unavailable, fixture_tools()).await;
        assert!(printed.contains("real model sessions"), "{printed}");
        assert!(
            printed.contains("1 task(s) x 3 trial(s) x 2 arms x 120 s per turn"),
            "{printed}"
        );
        assert!(printed.contains("wall clock: 30 min"), "{printed}");
        assert!(
            printed.contains("Nothing ran: confirm on a terminal"),
            "{printed}"
        );
        let mut declined = std::io::Cursor::new(b"n\n".to_vec());
        let printed = run_uplift_command(
            root,
            &identity,
            Confirmation::Prompt(&mut declined),
            fixture_tools(),
        )
        .await;
        assert!(printed.contains("Run this uplift run? [y/N]") && printed.contains("Nothing ran."));

        assert!(results(root, &candidate).is_empty());
        assert!(localpilot_localmind::run_statuses(root).is_empty());
        let mut out = Vec::new();
        status(root, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "No uplift runs.\n");

        // A project that has not enabled it never gets as far as the screen.
        commit(
            root,
            &[(
                ".localpilot.toml",
                &config_text(&check()).replace("uplift = true\n", ""),
            )],
            "turn uplift off",
        );
        let printed = run_uplift_command(root, &identity, Confirmation::Yes, fixture_tools()).await;
        assert!(
            printed.contains("Nothing ran: uplift is off for this project"),
            "{printed}"
        );
        assert!(!printed.contains("real model sessions"));
    }

    /// The whole command through the real `localbench`, a stand-in solver and
    /// the permission engine. Runs only when `LOCALPILOT_TEST_LOCALBENCH` names
    /// a `localbench` with the per-arm surface.
    #[tokio::test]
    async fn a_confirmed_uplift_run_reaches_review_through_the_real_localbench() {
        let Some(localbench) = std::env::var_os("LOCALPILOT_TEST_LOCALBENCH") else {
            eprintln!(
                "NOTICE: LOCALPILOT_TEST_LOCALBENCH is not set; the real-binary run was skipped"
            );
            return;
        };
        let (dir, candidate) = project();
        let root = dir.path();
        let identity = candidate.content_identity();
        approve_fixture_tasks(root, &identity).await;

        let tools_dir = tempfile::tempdir().unwrap();
        let solver = if cfg!(windows) {
            let path = tools_dir.path().join("solver.cmd");
            std::fs::write(&path, WINDOWS_SOLVER).unwrap();
            path
        } else {
            let path = tools_dir.path().join("solver.sh");
            std::fs::write(&path, UNIX_SOLVER).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            path
        };
        let tools = localpilot_localmind::UpliftTools {
            localbench: localbench.to_string_lossy().into_owned(),
            solver: solver.to_string_lossy().into_owned(),
        };

        // Confirmed at a prompt, under the Default profile: every localbench
        // command is an Ask, answered only because it was the one shown.
        let mut yes = std::io::Cursor::new(b"y\n".to_vec());
        let printed =
            run_uplift_command(root, &identity, Confirmation::Prompt(&mut yes), tools).await;

        assert!(printed.contains("Uplift Supported"), "{printed}");
        assert!(
            printed.contains("wall time:") && printed.contains("RAM: not measured"),
            "{printed}"
        );
        let stored = results(root, &candidate);
        assert_eq!(stored, vec![(LabVerdict::Supported, Vec::new())]);
        let (_, in_review) = lab_candidate(root, &identity).unwrap().unwrap();
        assert_eq!(
            in_review.experiments[0].tier,
            localmind_core::EvidenceTier::Uplift
        );
        in_review.experiments[0].validate(&in_review).unwrap();

        let mut out = Vec::new();
        status(root, &mut out).unwrap();
        let shown = String::from_utf8(out).unwrap();
        assert!(
            shown.contains("Supported") && shown.contains("fixture-model"),
            "{shown}"
        );
        let mut listing = Vec::new();
        list(root, &mut listing).unwrap();
        assert!(String::from_utf8(listing)
            .unwrap()
            .contains("Uplift Supported"));
        assert!(localpilot_localmind::memory_list_readonly(root)
            .unwrap()
            .is_empty());
    }

    const WINDOWS_SOLVER: &str = "@echo off\r\n\
if not exist .localpilot\\sessions mkdir .localpilot\\sessions\r\n\
set ID=\r\n\
for %%f in (.localmind\\memory\\project\\*.md) do set ID=%%~nf\r\n\
if \"%ID%\"==\"\" goto none\r\n\
echo {\"kind\":{\"type\":\"memories_used\",\"memories\":[{\"id\":\"%ID%\",\"score\":5,\"layer\":\"memory\"}]}}> .localpilot\\sessions\\turn.jsonl\r\n\
echo Run foo db sync first.\r\n\
exit /b 0\r\n\
:none\r\n\
echo {\"kind\":{\"type\":\"turn_done\"}}> .localpilot\\sessions\\turn.jsonl\r\n\
echo I am not sure.\r\n";

    const UNIX_SOLVER: &str = "#!/bin/sh\n\
mkdir -p .localpilot/sessions\n\
file=$(ls .localmind/memory/project/*.md 2>/dev/null | head -n 1)\n\
if [ -n \"$file\" ]; then\n\
  id=$(basename \"$file\" .md)\n\
  printf '{\"kind\":{\"type\":\"memories_used\",\"memories\":[{\"id\":\"%s\",\"score\":5,\"layer\":\"memory\"}]}}\\n' \"$id\" > .localpilot/sessions/turn.jsonl\n\
  echo 'Run foo db sync first.'\n\
else\n\
  printf '{\"kind\":{\"type\":\"turn_done\"}}\\n' > .localpilot/sessions/turn.jsonl\n\
  echo 'I am not sure.'\n\
fi\n";

    fn printed(run: impl FnOnce(&mut Vec<u8>) -> anyhow::Result<()>) -> String {
        let mut out = Vec::new();
        run(&mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// Asking for a rerun writes a note. It starts nothing, it does not stand
    /// in for the tier's opt-in or its confirmation, and it stays open until
    /// the run it asked for has actually produced a result.
    #[tokio::test]
    async fn a_rerun_request_is_shown_and_starts_nothing_until_a_person_runs_it() {
        let (dir, candidate) = project();
        let root = dir.path();
        let identity = candidate.content_identity();

        let mut out = Vec::new();
        assert!(
            rerun(root, &identity, "logic", "ada", None, false, &mut out)
                .unwrap_err()
                .to_string()
                .contains("use `replay` or `uplift`")
        );
        assert!(rerun(root, &identity, "replay", " ", None, false, &mut out)
            .unwrap_err()
            .to_string()
            .contains("named reviewer"));

        for tier in ["uplift", "replay"] {
            let text = printed(|out| {
                rerun(
                    root,
                    &identity,
                    tier,
                    "ada",
                    Some("look again".to_string()),
                    false,
                    out,
                )
            });
            assert!(text.contains("Nothing ran."), "{text}");
            assert!(text.contains("is not a confirmation"), "{text}");
        }
        assert!(
            results(root, &candidate).is_empty(),
            "a request runs nothing"
        );
        assert!(localpilot_localmind::run_statuses(root).is_empty());
        let listing = printed(|out| list(root, out));
        assert!(
            listing.contains("rerun requested: Uplift by ada (look again)"),
            "{listing}"
        );
        assert!(
            listing.contains("rerun requested: Replay by ada"),
            "{listing}"
        );

        // The request authorizes nothing: an uplift run still needs its approved
        // tasks, and a Replay run still needs its confirmation.
        let refused = run_uplift_command(root, &identity, Confirmation::Yes, fixture_tools()).await;
        assert!(refused.contains("Nothing ran"), "{refused}");
        let unconfirmed = run(root, Confirmation::Unavailable).await;
        assert!(unconfirmed.contains("Nothing ran"), "{unconfirmed}");
        assert!(results(root, &candidate).is_empty());
        let listing = printed(|out| list(root, out));
        assert!(
            listing.contains("rerun requested: Uplift"),
            "a refused run leaves the request"
        );
        assert!(listing.contains("rerun requested: Replay"), "{listing}");

        // A person runs Replay: the request for that tier is closed, the other
        // stays.
        let ran = run(root, Confirmation::Yes).await;
        assert!(ran.contains("Replay Valid"), "{ran}");
        let listing = printed(|out| list(root, out));
        assert!(!listing.contains("rerun requested: Replay"), "{listing}");
        assert!(listing.contains("rerun requested: Uplift"), "{listing}");

        let text = printed(|out| rerun(root, &identity, "uplift", "ada", None, true, out));
        assert!(text.contains("Withdrew the Uplift rerun request"), "{text}");
        assert!(!printed(|out| list(root, out)).contains("rerun requested"));
    }

    /// A rewritten lesson is history to the lab: its results stay with it and
    /// stay visible, nothing runs against it again, and the rewrite starts with
    /// no results and no lab record.
    #[tokio::test]
    async fn a_rewritten_lesson_is_history_to_the_lab_and_the_rewrite_starts_untested() {
        let (dir, candidate) = project();
        let root = dir.path();
        let identity = candidate.content_identity();
        assert!(run(root, Confirmation::Yes).await.contains("Replay Valid"));

        let text = printed(|out| {
            crate::learning_cmd::review_rewrite(
                root,
                "retro-1",
                &localmind_core::LessonRevision {
                    summary: Some("Write the state file in the setup step".to_string()),
                    cause: Some("the setup step never wrote the state file".to_string()),
                    ..localmind_core::LessonRevision::default()
                },
                "ada",
                None,
                out,
            )
        });
        assert!(
            text.contains("retro-1 -> rewritten as retro-1-r1 (accepted, untested)"),
            "{text}"
        );
        assert!(
            text.contains("1 lab result(s) describe the old text"),
            "{text}"
        );

        // The original keeps its result, marked as history.
        let listing = printed(|out| list(root, out));
        assert!(listing.contains("Replay Valid"), "{listing}");
        assert!(listing.contains("history (Merged)"), "{listing}");
        assert_eq!(
            results(root, &candidate),
            vec![(LabVerdict::Valid, Vec::new())]
        );

        // The rewrite carries none, and the lab has no record of it.
        let queue = localmind_store::ReviewQueue::open_project(root).unwrap();
        let revised = queue
            .get(&localmind_core::ReviewItemId::new("retro-1-r1"))
            .unwrap()
            .unwrap();
        assert!(revised.candidate.experiments.is_empty());
        assert_eq!(
            revised.candidate.revises.as_deref(),
            Some(identity.as_str())
        );
        assert!(!listing.contains(&revised.candidate.content_identity()));

        // Nothing runs against history, by any lab command.
        let replayed = run(root, Confirmation::Yes).await;
        assert!(replayed.contains("is history (Merged)"), "{replayed}");
        assert!(replayed.contains("Nothing to replay."), "{replayed}");
        let mut out = Vec::new();
        assert!(
            rerun(root, &identity, "replay", "ada", None, false, &mut out)
                .unwrap_err()
                .to_string()
                .contains("no longer a live lesson")
        );
        let drafted = tasks_draft(
            root,
            &identity,
            "local-model",
            &localpilot_llm::FakeProvider::new().text("{}"),
            &mut out,
        )
        .await;
        assert!(drafted.unwrap_err().to_string().contains("is history"));
        let uplifted = uplift(
            root,
            &identity,
            &uplift_args(),
            Confirmation::Yes,
            &engine(),
            fixture_tools(),
            None,
            &CancelSignal::new(),
            &mut out,
        )
        .await;
        assert!(uplifted.unwrap_err().to_string().contains("is history"));
        assert_eq!(results(root, &candidate).len(), 1);
    }

    /// The split commands: a model drafts, the draft changes nothing, and a
    /// named reviewer's approval puts untested parts in review.
    #[tokio::test]
    async fn a_split_is_drafted_shown_and_approved_from_the_command_line() {
        let (dir, candidate) = project();
        let root = dir.path();
        assert!(run(root, Confirmation::Yes).await.contains("Replay Valid"));
        let reply = r#"{"parts":[
            "Write the state file before running the check",
            "Make the check fail clearly when the state file is missing"
        ]}"#;

        let mut out = Vec::new();
        crate::learning_cmd::split_draft(
            root,
            "retro-1",
            "local-model",
            &localpilot_llm::FakeProvider::new().text(reply),
            &mut out,
        )
        .await
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("Drafted 2 part(s) for retro-1 with local-model"),
            "{text}"
        );
        assert!(text.contains("A draft changes nothing."), "{text}");

        let shown = printed(|out| crate::learning_cmd::split_show(root, "retro-1", out));
        assert!(
            shown.contains("Draft (2 part(s), drafted by local-model):"),
            "{shown}"
        );
        assert!(!shown.contains("cannot be approved"), "{shown}");
        let queue = localmind_store::ReviewQueue::open_project(root).unwrap();
        assert_eq!(queue.list().unwrap().len(), 1, "a draft changes nothing");

        let approved =
            printed(|out| crate::learning_cmd::split_approve(root, "retro-1", "ada", None, out));
        assert!(
            approved.contains("retro-1 -> split into 2 pending item(s) by ada"),
            "{approved}"
        );
        assert!(
            approved.contains("Lab results stay with retro-1."),
            "{approved}"
        );

        let items = queue.list().unwrap();
        assert_eq!(items.len(), 3);
        for item in items.iter().filter(|item| item.id.as_str() != "retro-1") {
            assert_eq!(item.state, localmind_core::ReviewState::Pending);
            assert!(
                item.candidate.experiments.is_empty(),
                "no result is inherited"
            );
            assert_eq!(
                item.candidate.revises.as_deref(),
                Some(candidate.content_identity().as_str())
            );
        }
        assert!(printed(|out| list(root, out)).contains("history (Merged)"));
        assert!(localpilot_localmind::memory_list_readonly(root)
            .unwrap()
            .is_empty());
    }

    /// The down-weight path stays off unless a project turns it on.
    #[test]
    fn outcome_downweighting_is_off_by_default() {
        assert!(
            !localpilot_config::Config::default()
                .memory
                .outcome_downweight
        );
    }

    /// `learning review show` is where a reviewer reads a lesson: both cards,
    /// then the lab's part — what can run, open rerun requests, and that
    /// starting a run is a separate, confirmed command.
    #[tokio::test]
    async fn review_show_carries_the_cards_and_the_labs_part() {
        let (dir, candidate) = project();
        let root = dir.path();
        let identity = candidate.content_identity();
        let show =
            |root: &Path| printed(|out| crate::learning_cmd::review_show(root, "retro-1", out));

        let before = show(root);
        assert!(
            before.contains("Hindsight\n  intended: Write the state"),
            "{before}"
        );
        assert!(
            before.contains("cause: the state file had not been written yet"),
            "{before}"
        );
        assert!(
            before.contains("Not tested. Most lessons are not"),
            "{before}"
        );
        assert!(
            before.contains(&format!("Lab\n  lesson identity: {identity}")),
            "{before}"
        );
        assert!(
            before.contains("can run: Replay, on the failing commit and its fix"),
            "{before}"
        );

        assert!(run(root, Confirmation::Yes).await.contains("Replay Valid"));
        printed(|out| {
            rerun(
                root,
                &identity,
                "uplift",
                "ada",
                Some("check it helps".to_string()),
                false,
                out,
            )
        });
        let after = show(root);
        assert!(after.contains("1. Replay Valid"), "{after}");
        assert!(
            after.contains("Replay re-runs the project's own check on the commits the lesson came from; it does not measure whether the lesson helps."),
            "{after}"
        );
        assert!(
            after.contains("from: a failing commit and the commit that fixed it"),
            "{after}"
        );
        assert!(after.contains("existed before the lesson"), "{after}");
        assert!(
            after.contains("— available"),
            "the run receipt is still retained: {after}"
        );
        assert!(
            after.contains("rerun requested: uplift by ada (check it helps) — not run."),
            "{after}"
        );
        assert!(
            after.contains("it shows what will run and asks first"),
            "{after}"
        );

        // Retention removes the receipt; the result stands and says so.
        std::fs::remove_dir_all(root.join(".localpilot").join("lab").join("runs")).unwrap();
        assert!(
            show(root).contains("no longer retained; the result itself still stands"),
            "a swept detail is said in words"
        );
    }
}
