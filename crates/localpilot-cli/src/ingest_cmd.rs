use std::io::Write;
use std::path::Path;

use localpilot_config::{CliOverrides, ConfigPaths};
use localpilot_localmind::{IngestJob, JobStatus, RunMode};

/// Print an ingestion preview.
///
/// # Errors
/// Returns an error if config cannot be loaded or discovery fails.
pub fn preview(project_root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let config = load_ingest_config(project_root)?;
    let manifest = localpilot_localmind::ingest_preview(project_root, &config)?;
    writeln!(
        out,
        "candidate files: {}",
        manifest.estimates.candidate_files
    )?;
    writeln!(out, "skipped files: {}", manifest.estimates.skipped_files)?;
    writeln!(
        out,
        "candidate bytes: {}",
        manifest.estimates.candidate_bytes
    )?;
    writeln!(
        out,
        "estimated tokens: {}",
        manifest.estimates.token_estimate
    )?;
    for entry in manifest.entries.iter().take(50) {
        writeln!(
            out,
            "{}\t{:?}\t{}\t{}",
            entry.path,
            entry.status,
            entry.size_bytes,
            entry.skip_reason.as_deref().unwrap_or("")
        )?;
    }
    Ok(())
}

/// Run or refresh ingestion.
///
/// # Errors
/// Returns an error if config cannot be loaded or ingestion fails.
pub fn run(project_root: &Path, mode: RunMode, out: &mut dyn Write) -> anyhow::Result<()> {
    let config = load_ingest_config(project_root)?;
    let summary = localpilot_localmind::ingest_run_with_progress(
        project_root,
        &config,
        mode,
        &|| false,
        &mut |stage| {
            if let Some(line) = progress_line(stage) {
                let _ = writeln!(out, "{line}");
            }
        },
    )?;
    writeln!(out, "status: {}", job_status(summary.job.status))?;
    writeln!(out, "files: {}", summary.job.completed_files)?;
    writeln!(out, "skipped: {}", summary.job.skipped_files)?;
    writeln!(out, "chunks: {}", summary.chunks_written)?;
    Ok(())
}

/// A one-line stage banner for the non-interactive `ingest run`/`refresh`
/// commands, so a batch run shows what it is doing instead of sitting silent.
/// Per-file `Parsing` ticks and the terminal `Completed` marker are left to the
/// caller's summary, so stdout stays compact.
fn progress_line(stage: localpilot_localmind::IngestProgress) -> Option<String> {
    use localpilot_localmind::IngestProgress;
    match stage {
        IngestProgress::Discovering => Some("discovering files…".to_string()),
        IngestProgress::Discovered {
            candidates,
            skipped,
        } => Some(format!(
            "discovered {candidates} candidate file(s) ({skipped} skipped)"
        )),
        IngestProgress::Indexing => Some("indexing project context…".to_string()),
        IngestProgress::Writing => Some("writing index…".to_string()),
        IngestProgress::Parsing { .. } | IngestProgress::Completed { .. } => None,
    }
}

/// Print current ingestion status, including what the next run would do —
/// resume an incomplete job, rebuild from scratch, or nothing when up to date.
///
/// # Errors
/// Returns an error if state cannot be read.
pub fn status(project_root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    match localpilot_localmind::ingest_status(project_root)? {
        Some(job) => {
            render_job(&job, out)?;
            let has_index = localpilot_localmind::has_chunk_index(project_root);
            let next = match localpilot_localmind::planned_run_mode(Some(&job), has_index) {
                None => "up to date — next session does not rebuild",
                Some(RunMode::Refresh) => "incomplete — next run resumes (refresh)",
                Some(RunMode::Full) => "incomplete — next run rebuilds (full)",
            };
            writeln!(out, "next: {next}")?;
        }
        None => writeln!(out, "no ingest job")?,
    }
    Ok(())
}

/// Continue an incomplete ingest job from the chunks already persisted, instead
/// of restarting a full walk. Resolves the same resume-vs-fresh decision the
/// session-open trigger uses, then runs it to completion in the foreground.
///
/// # Errors
/// Returns an error if config cannot be loaded or ingestion fails.
pub fn resume(project_root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let Some(job) = localpilot_localmind::ingest_status(project_root)? else {
        writeln!(out, "no ingest job to resume")?;
        return Ok(());
    };
    let has_index = localpilot_localmind::has_chunk_index(project_root);
    match localpilot_localmind::planned_run_mode(Some(&job), has_index) {
        None => {
            writeln!(
                out,
                "ingest job already completed; run `localpilot ingest refresh` to update"
            )?;
        }
        Some(mode) => {
            let config = load_ingest_config(project_root)?;
            let summary = localpilot_localmind::ingest_run(project_root, &config, mode)?;
            writeln!(out, "resumed in {} mode", run_mode_label(mode))?;
            writeln!(out, "status: {}", job_status(summary.job.status))?;
            writeln!(out, "files: {}", summary.job.completed_files)?;
            writeln!(out, "chunks: {}", summary.chunks_written)?;
        }
    }
    Ok(())
}

/// Set a control state on the current job.
///
/// # Errors
/// Returns an error if state cannot be updated.
pub fn control(
    project_root: &Path,
    action: ControlAction,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let job = match action {
        ControlAction::Pause => localpilot_localmind::ingest_pause(project_root)?,
        ControlAction::Cancel => localpilot_localmind::ingest_cancel(project_root)?,
    };
    match job {
        Some(job) => render_job(&job, out)?,
        None => writeln!(out, "no ingest job")?,
    }
    Ok(())
}

/// Rebuild derived ingestion state.
///
/// # Errors
/// Returns an error if state cannot be deleted.
pub fn rebuild(project_root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    localpilot_localmind::ingest_rebuild(project_root)?;
    writeln!(out, "deleted derived ingestion state")?;
    Ok(())
}

/// Print skipped files.
///
/// # Errors
/// Returns an error if the manifest cannot be read.
pub fn skipped(project_root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    for entry in localpilot_localmind::ingest_skipped(project_root)? {
        writeln!(
            out,
            "{}\t{:?}\t{}",
            entry.path,
            entry.status,
            entry.skip_reason.as_deref().unwrap_or("")
        )?;
    }
    Ok(())
}

/// Add an include or exclude rule.
///
/// # Errors
/// Returns an error if config cannot be updated.
pub fn rule(
    project_root: &Path,
    action: RuleAction,
    path: &Path,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let rule = match action {
        RuleAction::Include => localpilot_localmind::ingest_include(project_root, path)?,
        RuleAction::Exclude => localpilot_localmind::ingest_exclude(project_root, path)?,
    };
    writeln!(out, "{} {}", rule_action(action), rule)?;
    Ok(())
}

/// Forget derived knowledge for a path or id.
///
/// # Errors
/// Returns an error if state cannot be updated.
pub fn forget(project_root: &Path, target: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    let removed = localpilot_localmind::ingest_forget(project_root, target)?;
    writeln!(out, "removed {removed} derived record(s)")?;
    Ok(())
}

/// List ingestion review items.
///
/// # Errors
/// Returns an error if review state cannot be read.
pub fn review(project_root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let items = localpilot_localmind::ingest_review_items(project_root)?;
    if items.is_empty() {
        writeln!(out, "no ingestion review items")?;
        return Ok(());
    }
    for item in items {
        writeln!(out, "{}\t{}\t{}", item.id, item.kind, item.title)?;
    }
    Ok(())
}

/// Enqueue an ingestion item into LocalMind review.
///
/// # Errors
/// Returns an error if promotion fails.
pub fn promote(project_root: &Path, id: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    let inserted = localpilot_localmind::ingest_promote(project_root, id)?;
    writeln!(out, "queued {inserted} review item(s)")?;
    Ok(())
}

/// Search ingested knowledge.
///
/// # Errors
/// Returns an error if chunks cannot be searched.
pub fn knowledge_search(
    project_root: &Path,
    query: &str,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    for hit in localpilot_localmind::knowledge_search(project_root, query)? {
        writeln!(
            out,
            "{}:{}-{}\tscore {}\t{}",
            hit.path, hit.start_line, hit.end_line, hit.score, hit.snippet
        )?;
    }
    Ok(())
}

/// Build a task context pack.
///
/// # Errors
/// Returns an error if the pack cannot be built.
pub fn knowledge_pack(project_root: &Path, task: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    let pack = localpilot_localmind::build_pack(project_root, task, 4_000)?;
    writeln!(out, "task: {}", pack.task)?;
    writeln!(
        out,
        "budget: {}/{} tokens",
        pack.token_estimate, pack.token_budget
    )?;
    writeln!(
        out,
        "reserves: manual-pin n/a  accepted-memory {}  recent-session {}  ingest {}  code-graph {}",
        pack.accepted_memory_budget,
        pack.recent_session_budget,
        pack.ingest_budget,
        pack.code_graph_budget,
    )?;

    writeln!(out, "included ({}):", pack.entries.len())?;
    for entry in &pack.entries {
        writeln!(
            out,
            "  [{}] {} (score {}, {} tok) — {}",
            pack_source_label(entry.source),
            entry.path.as_deref().unwrap_or(&entry.id),
            entry.signals.final_score,
            entry.token_estimate,
            entry.reason,
        )?;
    }

    if !pack.skipped_entries.is_empty() {
        writeln!(out, "skipped near-misses ({}):", pack.skipped_entries.len())?;
        for entry in &pack.skipped_entries {
            writeln!(
                out,
                "  [{}] {} (score {}) — {}",
                pack_source_label(entry.source),
                entry.path.as_deref().unwrap_or(&entry.id),
                entry.signals.final_score,
                entry.reason,
            )?;
        }
    }
    Ok(())
}

fn pack_source_label(source: localpilot_localmind::PackSource) -> &'static str {
    match source {
        localpilot_localmind::PackSource::ManualPin => "pin",
        localpilot_localmind::PackSource::AcceptedMemory => "memory",
        localpilot_localmind::PackSource::RecentSession => "session",
        localpilot_localmind::PackSource::Ingest => "ingest",
        localpilot_localmind::PackSource::CodeGraph => "graph",
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ControlAction {
    Pause,
    Cancel,
}

#[derive(Debug, Clone, Copy)]
pub enum RuleAction {
    Include,
    Exclude,
}

pub(crate) fn load_ingest_config(
    project_root: &Path,
) -> anyhow::Result<localpilot_config::IngestConfig> {
    Ok(localpilot_config::load(
        &ConfigPaths::standard(project_root),
        &CliOverrides::default(),
    )?
    .ingest)
}

fn render_job(job: &IngestJob, out: &mut dyn Write) -> anyhow::Result<()> {
    writeln!(out, "status: {}", job_status(job.status))?;
    writeln!(out, "run: {}", job.run_id)?;
    writeln!(out, "mode: {}", job.mode)?;
    writeln!(out, "queued: {}", job.queued_files)?;
    writeln!(out, "completed: {}", job.completed_files)?;
    writeln!(out, "failed: {}", job.failed_files)?;
    writeln!(out, "skipped: {}", job.skipped_files)?;
    if let Some(message) = &job.message {
        writeln!(out, "message: {message}")?;
    }
    Ok(())
}

fn job_status(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Queued => "queued",
        JobStatus::Running => "running",
        JobStatus::Paused => "paused",
        JobStatus::Cancelled => "cancelled",
        JobStatus::Failed => "failed",
        JobStatus::Completed => "completed",
    }
}

fn rule_action(action: RuleAction) -> &'static str {
    match action {
        RuleAction::Include => "included",
        RuleAction::Exclude => "excluded",
    }
}

fn run_mode_label(mode: RunMode) -> &'static str {
    match mode {
        RunMode::Full => "full",
        RunMode::Refresh => "refresh",
    }
}
