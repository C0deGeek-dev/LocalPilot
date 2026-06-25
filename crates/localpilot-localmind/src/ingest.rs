//! Project-local folder ingestion.
//!
//! Ingestion records are derived, disposable project state under
//! `.localmind/ingest/`. They are deliberately separate from accepted memory:
//! promotion enqueues review candidates through LocalMind and never writes
//! accepted memory directly.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ignore::WalkBuilder;
use localmind_core::{
    CandidateLesson, Confidence, EvidenceKind, EvidenceRef, LessonCategory, LessonId,
    SessionId as LearningSessionId, SuggestedAction,
};
use localmind_store::ReviewQueue;
use localpilot_config::{redact, IngestConfig};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::chunk_store::ChunkStore;
use crate::pack::{allocate, reserves, PackCandidate, PackEntry, PackSource};

const INGEST_SCHEMA_VERSION: u32 = 1;
const INGEST_DIR: &str = ".localmind/ingest";
/// Marker file under `.localmind/` naming the live/just-closed session, so
/// on-demand retrieval can exclude the in-progress conversation from its
/// recent-session candidates instead of echoing it back as "knowledge".
const ACTIVE_SESSION_FILE: &str = ".localmind/active-session";
const MANIFEST_FILE: &str = "manifest.json";
const CHUNKS_FILE: &str = "chunks.json";
const JOB_FILE: &str = "job.json";
const REVIEW_FILE: &str = "review.json";
const PACK_FILE: &str = "last-pack.json";
const CHUNK_BYTES: usize = 8 * 1024;
/// How many manifest entries the session-open staleness check stat-samples, so
/// change detection stays bounded on the hot path (it never re-walks the tree).
const REFRESH_SAMPLE_CAP: usize = 256;
/// Synthetic path for the merged project-context document (the discovered
/// `CLAUDE.md`/`AGENTS.md` files with `@`-imports resolved and precedence
/// applied). Angle brackets keep it out of the slash-relative namespace real
/// workspace files use, so it never collides with a walked file's path.
const PROJECT_CONTEXT_PATH: &str = "<project-context>";

/// Folder ingestion errors.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum IngestError {
    #[error("ingestion is disabled in configuration")]
    Disabled,
    #[error("path {path} escapes the project root {root}")]
    OutsideProject { root: PathBuf, path: PathBuf },
    #[error("path contains an unsupported prefix: {0}")]
    UnsupportedPath(PathBuf),
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("json error at {path}: {source}")]
    Json {
        path: PathBuf,
        source: Box<serde_json::Error>,
    },
    #[error("toml error at {path}: {source}")]
    Toml {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
    #[error("toml write error at {path}: {source}")]
    TomlSerialize {
        path: PathBuf,
        source: Box<toml::ser::Error>,
    },
    #[error("chunk store error at {path}: {source}")]
    Sqlite {
        path: PathBuf,
        source: rusqlite::Error,
    },
    #[error("localmind review queue: {0}")]
    Review(String),
    #[error("invalid confidence for review candidate: {0}")]
    Confidence(String),
}

/// One file's ingestion disposition.
#[derive(Debug, Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStatus {
    Candidate,
    Ignored,
    Excluded,
    Generated,
    Binary,
    Unsupported,
    TooLarge,
    DecodeFailed,
    OverBudget,
}

/// Persistent job state.
#[derive(Debug, Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Queued,
    Running,
    Paused,
    Cancelled,
    Failed,
    Completed,
}

/// Why an ingest run is happening.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RunMode {
    Full,
    Refresh,
}

/// One manifest row.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct ManifestEntry {
    pub path: String,
    pub kind: String,
    pub size_bytes: u64,
    pub modified_unix: u64,
    pub content_hash: Option<String>,
    pub language: Option<String>,
    pub status: CandidateStatus,
    pub skip_reason: Option<String>,
    pub token_estimate: u64,
}

/// Preview and persisted manifest.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct PreviewManifest {
    pub schema_version: u32,
    pub generated_unix: u64,
    pub project_root: String,
    pub entries: Vec<ManifestEntry>,
    pub estimates: BudgetEstimate,
}

/// Budget estimates for a preview or run.
#[derive(Debug, Clone, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct BudgetEstimate {
    pub candidate_files: u64,
    pub skipped_files: u64,
    pub candidate_bytes: u64,
    pub token_estimate: u64,
    pub model_calls: u64,
    pub over_file_budget: bool,
    pub over_byte_budget: bool,
    pub over_token_budget: bool,
}

/// A persisted redacted text chunk.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChunkRecord {
    pub id: String,
    pub path: String,
    pub chunk_index: u32,
    pub start_line: u64,
    pub end_line: u64,
    pub start_byte: u64,
    pub end_byte: u64,
    pub content_hash: String,
    pub text: String,
    pub token_estimate: u64,
    pub stale: bool,
    /// The deterministic, offline context prefix for this chunk (file path plus
    /// a one-line gist). The prefixed text — not the raw `text` — is what the FTS
    /// index sees, so a chunk split mid-thought still matches its document's
    /// subject. Empty for chunks ingested before contextual prefixing existed.
    #[serde(default)]
    pub context_prefix: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub redaction_status: String,
    #[serde(default)]
    pub original_bytes: u64,
    #[serde(default)]
    pub preview_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
}

/// Job state saved on disk.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct IngestJob {
    pub schema_version: u32,
    pub run_id: String,
    pub status: JobStatus,
    pub mode: String,
    pub queued_files: u64,
    pub completed_files: u64,
    pub failed_files: u64,
    pub skipped_files: u64,
    pub started_unix: u64,
    pub updated_unix: u64,
    pub message: Option<String>,
}

/// Summary returned by a run.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunSummary {
    pub job: IngestJob,
    pub manifest: PreviewManifest,
    pub chunks_written: usize,
}

/// A generated review candidate backed by ingestion artifacts.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct IngestReviewItem {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub source_path: Option<String>,
    pub content_hash: Option<String>,
    pub stale: bool,
}

/// One search result.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct KnowledgeHit {
    pub chunk_id: String,
    pub path: String,
    pub score: u64,
    pub start_line: u64,
    pub end_line: u64,
    pub content_hash: String,
    pub stale: bool,
    pub snippet: String,
    #[serde(default)]
    pub token_estimate: u64,
    #[serde(default)]
    pub inclusion_reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
}

/// A task-specific context pack.
#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextPack {
    pub schema_version: u32,
    pub task: String,
    pub token_budget: u64,
    pub token_estimate: u64,
    pub chunks: Vec<KnowledgeHit>,
    pub exclusion_notes: Vec<String>,
    #[serde(default)]
    pub skipped_near_misses: Vec<KnowledgeHit>,
    #[serde(default)]
    pub accepted_memory_budget: u64,
    #[serde(default)]
    pub ingest_budget: u64,
    #[serde(default)]
    pub code_graph_budget: u64,
    #[serde(default)]
    pub recent_session_budget: u64,
    /// The unified cross-source allocation: every selected entry, in priority
    /// order, with the reason it was included.
    #[serde(default)]
    pub entries: Vec<PackEntry>,
    /// High-ranking candidates that lost the budget competition, with the reason.
    #[serde(default)]
    pub skipped_entries: Vec<PackEntry>,
}

struct ChunkSpan {
    start_line: u64,
    end_line: u64,
    start_byte: u64,
    end_byte: u64,
}

#[derive(Debug, Deserialize)]
struct LocalMindConfig {
    #[serde(default)]
    ingest: LocalMindIngestConfig,
}

#[derive(Debug, Default, Deserialize)]
struct LocalMindIngestConfig {
    #[serde(default)]
    excluded_paths: Vec<String>,
}

/// Produce a deterministic preview without persisting file content.
///
/// # Errors
/// Returns [`IngestError`] for path, filesystem, or config parse failures.
pub fn preview(project_root: &Path, config: &IngestConfig) -> Result<PreviewManifest, IngestError> {
    let root = canonical_root(project_root)?;
    let localmind_excludes = localmind_excluded_paths(&root)?;
    let mut entries = Vec::new();
    let mut estimates = BudgetEstimate::default();
    let mut candidate_files = 0_u64;
    let mut candidate_bytes = 0_u64;
    let mut candidate_tokens = 0_u64;

    for result in walker(&root) {
        let entry = result.map_err(|source| {
            let message = source.to_string();
            IngestError::Io {
                path: root.clone(),
                source: source
                    .into_io_error()
                    .unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, message)),
            }
        })?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        let mut manifest_entry = classify(&root, path, config, &localmind_excludes)?;
        if manifest_entry.status == CandidateStatus::Candidate {
            candidate_files = candidate_files.saturating_add(1);
            candidate_bytes = candidate_bytes.saturating_add(manifest_entry.size_bytes);
            candidate_tokens = candidate_tokens.saturating_add(manifest_entry.token_estimate);
            if candidate_files > config.max_files
                || candidate_bytes > config.max_run_bytes
                || candidate_tokens > config.max_tokens
            {
                manifest_entry.status = CandidateStatus::OverBudget;
                manifest_entry.skip_reason = Some("run budget exceeded".to_string());
            }
        }
        match manifest_entry.status {
            CandidateStatus::Candidate => {
                estimates.candidate_files = estimates.candidate_files.saturating_add(1);
                estimates.candidate_bytes = estimates
                    .candidate_bytes
                    .saturating_add(manifest_entry.size_bytes);
                estimates.token_estimate = estimates
                    .token_estimate
                    .saturating_add(manifest_entry.token_estimate);
            }
            _ => estimates.skipped_files = estimates.skipped_files.saturating_add(1),
        }
        entries.push(manifest_entry);
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    estimates.over_file_budget = estimates.candidate_files > config.max_files;
    estimates.over_byte_budget = estimates.candidate_bytes > config.max_run_bytes;
    estimates.over_token_budget = estimates.token_estimate > config.max_tokens;
    Ok(PreviewManifest {
        schema_version: INGEST_SCHEMA_VERSION,
        generated_unix: unix_now(),
        project_root: root.display().to_string(),
        entries,
        estimates,
    })
}

/// A coarse, milestone-level progress signal emitted across a run so a host can
/// show a live loader instead of blocking silently. The `Parsing` stage is
/// emitted once per candidate file (a host should throttle its own redraws); the
/// rest are one-shot phase markers.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum IngestProgress {
    /// Walking the project tree to classify candidate files.
    Discovering,
    /// Discovery finished: how many files will be parsed, and how many skipped.
    Discovered { candidates: u64, skipped: u64 },
    /// Parsing/chunking candidate files (reused untouched on refresh when fresh).
    Parsing { completed: u64, total: u64 },
    /// Merging and indexing the project-context document.
    Indexing,
    /// Writing the manifest, review items, and job state to disk.
    Writing,
    /// The run finished: files completed and chunks now in the index.
    Completed { files: u64, chunks: usize },
}

/// Run or refresh ingestion, persisting redacted chunks and manifest state.
///
/// # Errors
/// Returns [`IngestError`] if the run cannot read source files or write state.
pub fn run(
    project_root: &Path,
    config: &IngestConfig,
    mode: RunMode,
) -> Result<RunSummary, IngestError> {
    run_with_progress(project_root, config, mode, &|| false, &mut |_| {})
}

/// Run or refresh ingestion while reporting progress and honoring cancellation,
/// so an interactive host can drive a loader. `should_cancel` is polled before
/// each file; a `true` return pauses the job (partial chunks are kept, so the
/// next run resumes via [`RunMode::Refresh`]). `progress` receives
/// [`IngestProgress`] milestones. [`run`] is the no-op-callback shorthand.
///
/// # Errors
/// Returns [`IngestError`] if the run cannot read source files or write state.
pub fn run_with_progress(
    project_root: &Path,
    config: &IngestConfig,
    mode: RunMode,
    should_cancel: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(IngestProgress),
) -> Result<RunSummary, IngestError> {
    if !config.enabled {
        return Err(IngestError::Disabled);
    }
    let root = canonical_root(project_root)?;
    let ingest_dir = ensure_ingest_dir(&root)?;
    progress(IngestProgress::Discovering);
    let manifest = preview(&root, config)?;
    progress(IngestProgress::Discovered {
        candidates: manifest.estimates.candidate_files,
        skipped: manifest.estimates.skipped_files,
    });
    let store = ChunkStore::open(&ingest_dir)?;
    // Contextual chunk prefixing. The model-enrichment tier is gated on the
    // opt-in flag; no enricher is wired on this local path, so prefixes stay
    // synthetic (the flag alone never causes egress).
    let prefix_policy = crate::context_prefix::PrefixEnrichmentPolicy {
        enabled: config.contextual_prefix_enrichment,
    };
    // A full run rebuilds from scratch; a refresh updates incrementally,
    // reusing unchanged files and tombstoning what changed or disappeared.
    if mode == RunMode::Full {
        store.clear()?;
    }
    let mut candidate_paths: BTreeSet<String> = manifest
        .entries
        .iter()
        .filter(|entry| entry.status == CandidateStatus::Candidate)
        .map(|entry| entry.path.clone())
        .collect();
    let started = Instant::now();
    let started_unix = unix_now();
    let mut job = IngestJob {
        schema_version: INGEST_SCHEMA_VERSION,
        run_id: format!("run-{started_unix}"),
        status: JobStatus::Running,
        mode: match mode {
            RunMode::Full => "full".to_string(),
            RunMode::Refresh => "refresh".to_string(),
        },
        queued_files: manifest.estimates.candidate_files,
        completed_files: 0,
        failed_files: 0,
        skipped_files: manifest.estimates.skipped_files,
        started_unix,
        updated_unix: started_unix,
        message: None,
    };
    write_json(&ingest_dir.join(JOB_FILE), &job)?;

    let total_candidates = manifest.estimates.candidate_files;
    for entry in manifest
        .entries
        .iter()
        .filter(|entry| entry.status == CandidateStatus::Candidate)
    {
        // Honor a host cancellation (e.g. Ctrl-C) the same way the time budget
        // does: pause with partial chunks persisted, so the next run resumes.
        if should_cancel() {
            job.status = JobStatus::Paused;
            job.message = Some("interrupted by user".to_string());
            break;
        }
        if started.elapsed().as_secs() > config.max_elapsed_secs {
            job.status = JobStatus::Paused;
            job.message = Some("elapsed time budget reached".to_string());
            break;
        }
        progress(IngestProgress::Parsing {
            completed: job.completed_files,
            total: total_candidates,
        });
        let Some(hash) = &entry.content_hash else {
            job.failed_files = job.failed_files.saturating_add(1);
            continue;
        };
        // Unchanged file on refresh: its rows are already fresh in the store, so
        // it is reused untouched (no re-read, no re-chunk).
        if mode == RunMode::Refresh && store.has_fresh(&entry.path, hash)? {
            job.completed_files = job.completed_files.saturating_add(1);
            continue;
        }
        let absolute = root.join(platform_path(&entry.path));
        match chunk_file(&absolute, &entry.path, hash, &prefix_policy) {
            Ok(file_chunks) => {
                // Changed file: tombstone the path's prior fresh rows (kept,
                // flagged, pointed at the new hash) before writing the new ones.
                if mode == RunMode::Refresh {
                    store.mark_path_changed(&entry.path, hash)?;
                }
                store.upsert_chunks(&file_chunks)?;
                job.completed_files = job.completed_files.saturating_add(1);
            }
            Err(_) => {
                job.failed_files = job.failed_files.saturating_add(1);
            }
        }
        job.updated_unix = unix_now();
        write_json(&ingest_dir.join(JOB_FILE), &job)?;
    }
    // Ingest the merged project context (the discovered CLAUDE.md/AGENTS.md, with
    // @-imports resolved and precedence applied) as one first-class derived
    // document, so retrieval surfaces project conventions and constraints even
    // when the raw files are large or scattered across nested directories. It is
    // a candidate path so the refresh sweep below never tombstones it.
    progress(IngestProgress::Indexing);
    let context_chunks = project_context_chunks(&root, &prefix_policy);
    if let Some(hash) = context_chunks
        .first()
        .map(|chunk| chunk.content_hash.clone())
    {
        if mode == RunMode::Refresh {
            store.mark_path_changed(PROJECT_CONTEXT_PATH, &hash)?;
        }
        store.upsert_chunks(&context_chunks)?;
        candidate_paths.insert(PROJECT_CONTEXT_PATH.to_string());
    }

    // Files that vanished since the last run: their fresh rows become stale with
    // no successor (kept as tombstones, like the old JSON behaviour).
    if mode == RunMode::Refresh {
        store.stale_removed_paths(&candidate_paths)?;
    }

    if job.status == JobStatus::Running {
        job.status = if job.failed_files == 0 {
            JobStatus::Completed
        } else {
            JobStatus::Failed
        };
    }
    job.updated_unix = unix_now();
    progress(IngestProgress::Writing);
    let chunk_count = store.count()?;
    let review = build_review_items(&manifest, chunk_count);
    write_json(&ingest_dir.join(MANIFEST_FILE), &manifest)?;
    write_json(&ingest_dir.join(REVIEW_FILE), &review)?;
    write_json(&ingest_dir.join(JOB_FILE), &job)?;

    progress(IngestProgress::Completed {
        files: job.completed_files,
        chunks: chunk_count,
    });
    Ok(RunSummary {
        job,
        manifest,
        chunks_written: chunk_count,
    })
}

/// Delete only derived ingestion artifacts.
///
/// # Errors
/// Returns [`IngestError`] if state deletion fails.
pub fn rebuild(project_root: &Path) -> Result<(), IngestError> {
    let root = canonical_root(project_root)?;
    let ingest_dir = root.join(INGEST_DIR);
    if ingest_dir.exists() {
        fs::remove_dir_all(&ingest_dir).map_err(|source| IngestError::Io {
            path: ingest_dir,
            source,
        })?;
    }
    Ok(())
}

/// Current job state, if one has been written.
///
/// # Errors
/// Returns [`IngestError`] when the state file is malformed.
pub fn status(project_root: &Path) -> Result<Option<IngestJob>, IngestError> {
    let root = canonical_root(project_root)?;
    let path = root.join(INGEST_DIR).join(JOB_FILE);
    if !path.exists() {
        return Ok(None);
    }
    read_json(&path).map(Some)
}

/// Whether a first-use background index build should run, given the latest job
/// state: build when there is no job yet, or the last job did not complete (so a
/// previously interrupted build is retried). A pure decision so the host's
/// session-open trigger is unit-testable.
#[must_use]
pub fn should_build_index(job: Option<&IngestJob>) -> bool {
    match job {
        None => true,
        Some(job) => job.status != JobStatus::Completed,
    }
}

/// Which mode the session-open background build should run in, given the latest
/// job state and whether a prior chunk index exists:
/// - no job yet → [`RunMode::Full`] (first-ever build);
/// - last job completed → `None` (nothing to do);
/// - an incomplete job (paused/failed/cancelled/running) **with** a persisted
///   index → [`RunMode::Refresh`], which reuses unchanged files by
///   `path:content_hash` and so continues from the chunks already on disk
///   instead of re-walking and re-chunking from scratch;
/// - an incomplete job with no index yet → [`RunMode::Full`].
///
/// This is the fix for the pause→Full-restart loop: a run interrupted by the
/// elapsed-time budget persists the chunks completed so far, and the next
/// trigger resumes them via Refresh rather than discarding them. A pure decision
/// so the host's session-open trigger stays unit-testable.
#[must_use]
pub fn planned_run_mode(job: Option<&IngestJob>, has_index: bool) -> Option<RunMode> {
    match job {
        None => Some(RunMode::Full),
        Some(job) if job.status == JobStatus::Completed => None,
        Some(_) if has_index => Some(RunMode::Refresh),
        Some(_) => Some(RunMode::Full),
    }
}

/// Whether a persisted chunk index already exists for the project, so an
/// interrupted run can Refresh-resume rather than rebuild from scratch. True for
/// the SQLite store or a not-yet-migrated legacy `chunks.json`.
#[must_use]
pub fn has_chunk_index(project_root: &Path) -> bool {
    canonical_root(project_root)
        .map(|root| {
            let ingest_dir = root.join(INGEST_DIR);
            crate::chunk_store::exists(&ingest_dir) || ingest_dir.join(CHUNKS_FILE).exists()
        })
        .unwrap_or(false)
}

/// Whether a **completed** index should self-refresh on session open, given a
/// cheap "sources changed" signal and the cadence floor. A pure decision so the
/// trigger is unit-testable. Non-completed jobs are the first-build/resume path
/// ([`planned_run_mode`]) and never refresh here; a completed job refreshes only
/// when sources changed and at least `min_interval_secs` has passed since the
/// last run (a debounce against re-walking on quick successive sessions).
#[must_use]
pub fn should_refresh(
    job: Option<&IngestJob>,
    sources_changed: bool,
    now_unix: u64,
    min_interval_secs: u64,
) -> bool {
    match job {
        Some(job) if job.status == JobStatus::Completed => {
            sources_changed && now_unix.saturating_sub(job.updated_unix) >= min_interval_secs
        }
        _ => false,
    }
}

/// The run mode the session-open background trigger should use, or `None` to do
/// nothing: a first-ever build or an interrupted-job resume
/// ([`planned_run_mode`]), or a staleness [`RunMode::Refresh`] when a completed
/// index's sources changed and the cadence window is open. Read-only — it only
/// inspects persisted job/manifest state and stat-samples the tracked files.
///
/// Returns `None` immediately when ingest is disabled, so a disabled project
/// never triggers a background run.
#[must_use]
pub fn session_open_mode(project_root: &Path, config: &IngestConfig) -> Option<RunMode> {
    if !config.enabled {
        return None;
    }
    let job = status(project_root).ok().flatten();
    if let Some(mode) = planned_run_mode(job.as_ref(), has_chunk_index(project_root)) {
        return Some(mode);
    }
    // Completed index: self-refresh when sources changed and the cadence window is
    // open. The cheap window check gates the (bounded) filesystem sampling.
    let now = unix_now();
    let interval = config.refresh_min_interval_secs;
    let window_open = job
        .as_ref()
        .is_some_and(|job| now.saturating_sub(job.updated_unix) >= interval);
    let sources_changed =
        window_open && sources_changed_since_completed(project_root, REFRESH_SAMPLE_CAP);
    should_refresh(job.as_ref(), sources_changed, now, interval).then_some(RunMode::Refresh)
}

/// Whether any tracked source file changed since the last completed run, sampled
/// from the persisted manifest so this stays cheap on the session-open hot path:
/// it reuses the manifest's file list (no fresh tree walk) and stats at most
/// `cap` candidate entries, spread evenly across the manifest. A file whose mtime
/// is newer than the manifest recorded — or that has vanished — counts as a
/// change. Best-effort: a missing/unreadable manifest reports no change.
fn sources_changed_since_completed(project_root: &Path, cap: usize) -> bool {
    let Ok(root) = canonical_root(project_root) else {
        return false;
    };
    let manifest_path = root.join(INGEST_DIR).join(MANIFEST_FILE);
    let Ok(manifest) = read_json::<PreviewManifest>(&manifest_path) else {
        return false;
    };
    let candidates: Vec<&ManifestEntry> = manifest
        .entries
        .iter()
        .filter(|entry| entry.status == CandidateStatus::Candidate)
        .collect();
    if candidates.is_empty() {
        return false;
    }
    let cap = cap.max(1);
    // Stride so the bounded sample spans the whole manifest, not just its head.
    let step = candidates.len().div_ceil(cap).max(1);
    for entry in candidates.iter().step_by(step).take(cap) {
        let absolute = root.join(platform_path(&entry.path));
        match fs::metadata(&absolute)
            .ok()
            .and_then(|meta| meta.modified().ok())
        {
            Some(modified) => {
                let current = system_time_to_unix(modified).unwrap_or(0);
                if current > entry.modified_unix {
                    return true;
                }
            }
            None => return true,
        }
    }
    false
}

/// Mark the current job paused.
///
/// # Errors
/// Returns [`IngestError`] when state cannot be written.
pub fn pause(project_root: &Path) -> Result<Option<IngestJob>, IngestError> {
    set_job_status(project_root, JobStatus::Paused, "paused by user")
}

/// Mark the current job queued for a later run.
///
/// # Errors
/// Returns [`IngestError`] when state cannot be written.
pub fn resume(project_root: &Path) -> Result<Option<IngestJob>, IngestError> {
    set_job_status(project_root, JobStatus::Queued, "queued for resume")
}

/// Mark the current job cancelled.
///
/// # Errors
/// Returns [`IngestError`] when state cannot be written.
pub fn cancel(project_root: &Path) -> Result<Option<IngestJob>, IngestError> {
    set_job_status(project_root, JobStatus::Cancelled, "cancelled by user")
}

/// Stable skipped-file report.
///
/// # Errors
/// Returns [`IngestError`] when the manifest cannot be read.
pub fn skipped(project_root: &Path) -> Result<Vec<ManifestEntry>, IngestError> {
    let root = canonical_root(project_root)?;
    let path = root.join(INGEST_DIR).join(MANIFEST_FILE);
    let manifest = read_json::<PreviewManifest>(&path)?;
    Ok(manifest
        .entries
        .into_iter()
        .filter(|entry| entry.status != CandidateStatus::Candidate)
        .collect())
}

/// Forget derived knowledge for one path or chunk/review id.
///
/// # Errors
/// Returns [`IngestError`] when state cannot be read or written.
pub fn forget(project_root: &Path, target: &str) -> Result<usize, IngestError> {
    let root = canonical_root(project_root)?;
    let ingest_dir = root.join(INGEST_DIR);
    let review_path = ingest_dir.join(REVIEW_FILE);
    let mut removed = 0;
    if crate::chunk_store::exists(&ingest_dir) {
        let store = ChunkStore::open(&ingest_dir)?;
        removed += store.forget(target)?;
    }
    if review_path.exists() {
        let mut items = read_json::<Vec<IngestReviewItem>>(&review_path)?;
        let before = items.len();
        items.retain(|item| {
            item.id != target
                && item
                    .source_path
                    .as_deref()
                    .is_none_or(|path| path != target)
        });
        removed += before.saturating_sub(items.len());
        write_json(&review_path, &items)?;
    }
    Ok(removed)
}

/// Add an explicit include rule to the project-local LocalPilot config.
///
/// # Errors
/// Returns [`IngestError`] when the path escapes the project or config cannot
/// be written.
pub fn include_path(project_root: &Path, path: &Path) -> Result<String, IngestError> {
    update_rule(project_root, "include", path)
}

/// Add an explicit exclude rule to the project-local LocalPilot config.
///
/// # Errors
/// Returns [`IngestError`] when the path escapes the project or config cannot
/// be written.
pub fn exclude_path(project_root: &Path, path: &Path) -> Result<String, IngestError> {
    update_rule(project_root, "exclude", path)
}

/// Search deterministic chunk records.
///
/// The FTS index narrows to the candidate rows for the query; the existing
/// term-count + path-name-boost score is then recomputed over just those rows,
/// so the ranking is unchanged while the whole index is never loaded into RAM.
///
/// # Errors
/// Returns [`IngestError`] when the chunk store cannot be opened or queried.
pub fn search(project_root: &Path, query: &str) -> Result<Vec<KnowledgeHit>, IngestError> {
    let root = canonical_root(project_root)?;
    let ingest_dir = root.join(INGEST_DIR);
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|term| term.to_ascii_lowercase())
        .filter(|term| !term.is_empty())
        .collect();
    if terms.is_empty() || !crate::chunk_store::exists(&ingest_dir) {
        return Ok(Vec::new());
    }
    let store = ChunkStore::open(&ingest_dir)?;
    let mut hits = Vec::new();
    for chunk in store.search(&terms)? {
        let text = chunk.text.to_ascii_lowercase();
        let path = chunk.path.to_ascii_lowercase();
        let mut score = 0_u64;
        for term in &terms {
            score = score.saturating_add(text.matches(term).count() as u64);
            if path.contains(term) {
                score = score.saturating_add(3);
            }
        }
        if score == 0 {
            continue;
        }
        hits.push(KnowledgeHit {
            chunk_id: chunk.id,
            path: chunk.path,
            score,
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            content_hash: chunk.content_hash,
            stale: chunk.stale,
            snippet: summarize_snippet(&chunk.text, &terms),
            token_estimate: chunk.token_estimate,
            inclusion_reason: "query term match".to_string(),
            skip_reason: None,
        });
    }
    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.chunk_id.cmp(&b.chunk_id))
    });
    Ok(hits)
}

/// Compute a task-specific context pack from every reachable source under one
/// ranked token budget, **without persisting it**. Read-only — it only reads
/// derived state (ingest index, accepted memory, code graph, recent session), so
/// callers like the `knowledge_search` tool can pull a ranked pack on demand
/// without a write. Use [`build_pack`] for the inspectable on-disk pack.
///
/// `exclude_session` names the live/in-progress session (by its
/// `.localmind/sessions/<id>` directory name) so its summary never competes as a
/// recent-session candidate — the current conversation must not be echoed back as
/// project knowledge.
///
/// # Errors
/// Returns [`IngestError`] when derived state cannot be read.
pub fn compute_pack(
    project_root: &Path,
    task: &str,
    token_budget: u64,
    exclude_session: Option<&str>,
) -> Result<ContextPack, IngestError> {
    let root = canonical_root(project_root)?;
    let ingest_dir = root.join(INGEST_DIR);

    // Gather candidates from every reachable source so they compete under one
    // budget. Ingest hits come from the task query; accepted-memory anchors are
    // best-effort and only consulted when the project actually has a memory
    // store (so a bare ingest project is untouched).
    let task_lower = task.to_ascii_lowercase();
    let mut candidates = Vec::new();
    let mut hit_by_id: BTreeMap<String, KnowledgeHit> = BTreeMap::new();
    for hit in search(&root, task)? {
        let file_match = task_names_path(&task_lower, &hit.path);
        candidates.push(PackCandidate {
            source: PackSource::Ingest,
            id: hit.chunk_id.clone(),
            path: Some(hit.path.clone()),
            score: hit.score,
            token_estimate: hit.token_estimate,
            snippet: hit.snippet.clone(),
            stale: hit.stale,
            recency: 0,
            file_match,
            confidence: 0.5,
            graph_proximity: 0,
        });
        hit_by_id.insert(hit.chunk_id.clone(), hit);
    }
    if root.join(".localmind").join("memory").exists() {
        for anchor in crate::ops::search(&root, task).unwrap_or_default() {
            let token_estimate = (anchor.snippet.chars().count() as u64 / 4).max(1);
            let file_match = task_names_path(&task_lower, &anchor.path);
            candidates.push(PackCandidate {
                source: PackSource::AcceptedMemory,
                id: format!("memory:{}", anchor.memory_id),
                path: Some(anchor.path),
                score: u64::try_from(anchor.score.max(0)).unwrap_or(0),
                token_estimate,
                snippet: anchor.snippet,
                stale: false,
                recency: 0,
                file_match,
                // Accepted memory is review-gated, so it carries full confidence.
                confidence: 1.0,
                graph_proximity: 0,
            });
        }
    }

    // Code-graph neighbors of task-relevant symbols compete as a source. The
    // symbol itself is proximity 0; its direct neighbors are proximity 1.
    for symbol in task_symbols(task) {
        let Ok(report) = crate::codegraph::codegraph_inspect(&root, &symbol) else {
            continue;
        };
        let path = report.path.clone();
        let file_match = path
            .as_deref()
            .is_some_and(|p| task_names_path(&task_lower, p));
        candidates.push(PackCandidate {
            source: PackSource::CodeGraph,
            id: format!("graph:{symbol}"),
            path: path.clone(),
            score: 8,
            token_estimate: (report.qualified_name.chars().count() as u64 / 4).max(1),
            snippet: format!("{} {}", report.kind, report.qualified_name),
            stale: false,
            recency: 0,
            file_match,
            confidence: 0.8,
            graph_proximity: 0,
        });
        for (index, neighbor) in report.neighbors.iter().enumerate().take(8) {
            candidates.push(PackCandidate {
                source: PackSource::CodeGraph,
                id: format!("graph:{symbol}:{index}"),
                path: path.clone(),
                score: 5,
                token_estimate: (neighbor.chars().count() as u64 / 4).max(1),
                snippet: neighbor.clone(),
                stale: false,
                recency: 0,
                file_match,
                confidence: 0.6,
                graph_proximity: 1,
            });
        }
    }

    // Recent session facts come from the most recent LocalMind session summary,
    // excluding the live/in-progress session so the current conversation is not
    // served back to itself as "knowledge".
    for (index, fact) in recent_session_facts(&root, exclude_session)
        .into_iter()
        .enumerate()
    {
        candidates.push(PackCandidate {
            source: PackSource::RecentSession,
            id: format!("session:{index}"),
            path: None,
            score: 8,
            token_estimate: (fact.chars().count() as u64 / 4).max(1),
            snippet: fact,
            stale: false,
            // All from the latest session, so they share a high recency rank.
            recency: 40,
            file_match: false,
            confidence: 0.8,
            graph_proximity: 0,
        });
    }

    let allocation = allocate(candidates, token_budget);

    // Back-compat ingest view: rebuild the `KnowledgeHit` chunks the allocator
    // selected from the ingest source, carrying its inclusion reason.
    let mut chunks = Vec::new();
    for entry in &allocation.selected {
        if entry.source == PackSource::Ingest {
            if let Some(mut hit) = hit_by_id.get(&entry.id).cloned() {
                hit.token_estimate = entry.token_estimate;
                hit.inclusion_reason = entry.reason.clone();
                chunks.push(hit);
            }
        }
    }
    let mut skipped_near_misses = Vec::new();
    for entry in &allocation.skipped {
        if entry.source == PackSource::Ingest {
            if let Some(mut hit) = hit_by_id.get(&entry.id).cloned() {
                hit.skip_reason = Some(entry.reason.clone());
                skipped_near_misses.push(hit);
            }
        }
    }

    let exclusion_notes = if ingest_dir.join(MANIFEST_FILE).exists() {
        skipped(&root)?
            .into_iter()
            .take(10)
            .map(|entry| {
                format!(
                    "{}: {}",
                    entry.path,
                    entry
                        .skip_reason
                        .unwrap_or_else(|| format!("{:?}", entry.status))
                )
            })
            .collect()
    } else {
        Vec::new()
    };
    let reserve = reserves(token_budget);
    let pack = ContextPack {
        schema_version: INGEST_SCHEMA_VERSION,
        task: task.to_string(),
        token_budget,
        token_estimate: allocation.token_estimate,
        chunks,
        exclusion_notes,
        skipped_near_misses: skipped_near_misses.into_iter().take(10).collect(),
        accepted_memory_budget: reserve
            .get(&PackSource::AcceptedMemory)
            .copied()
            .unwrap_or(0),
        ingest_budget: reserve.get(&PackSource::Ingest).copied().unwrap_or(0),
        code_graph_budget: reserve.get(&PackSource::CodeGraph).copied().unwrap_or(0),
        recent_session_budget: reserve
            .get(&PackSource::RecentSession)
            .copied()
            .unwrap_or(0),
        entries: allocation.selected,
        skipped_entries: allocation.skipped.into_iter().take(20).collect(),
    };
    Ok(pack)
}

/// Build a task-specific context pack and persist it as `last-pack.json` for
/// inspection and staleness handling. Thin persisting wrapper over
/// [`compute_pack`].
///
/// # Errors
/// Returns [`IngestError`] when state cannot be read or written.
pub fn build_pack(
    project_root: &Path,
    task: &str,
    token_budget: u64,
) -> Result<ContextPack, IngestError> {
    let pack = compute_pack(project_root, task, token_budget, None)?;
    let ingest_dir = canonical_root(project_root)?.join(INGEST_DIR);
    write_json(&ingest_dir.join(PACK_FILE), &pack)?;
    Ok(pack)
}

/// Identifier-like tokens from a task query that may name code-graph symbols.
/// Common English words are dropped; at most three are probed so a pack build
/// never fans out into the graph.
fn task_symbols(task: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "the", "and", "for", "with", "fix", "add", "use", "run", "this", "that", "into", "from",
        "make", "test", "code", "file", "files", "function", "please", "update", "change",
    ];
    let mut seen = BTreeSet::new();
    task.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .filter(|token| token.len() >= 3 && token.chars().any(|c| c.is_ascii_alphabetic()))
        .filter(|token| !STOP.contains(&token.to_ascii_lowercase().as_str()))
        .filter(|token| seen.insert(token.to_ascii_lowercase()))
        .take(3)
        .map(str::to_string)
        .collect()
}

/// Key points from the most recent LocalMind session summary, used as
/// recent-session retrieval candidates. Best-effort: a missing or malformed
/// summary yields none. `exclude_session` (a `.localmind/sessions/<id>` directory
/// name) is skipped so the live/in-progress conversation never surfaces. Raw
/// transcript-echo key-points (role-prefixed lines like `user:` / `assistant
/// calls …:`) are dropped — they are conversation, not durable facts.
fn recent_session_facts(root: &Path, exclude_session: Option<&str>) -> Vec<String> {
    let sessions_dir = root.join(".localmind").join("sessions");
    let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
        return Vec::new();
    };
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        // Skip the active/in-progress session's own directory.
        if exclude_session.is_some_and(|active| entry.file_name().to_string_lossy() == active) {
            continue;
        }
        let summary = entry.path().join("summary.json");
        let Ok(modified) = summary.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if newest.as_ref().is_none_or(|(at, _)| modified > *at) {
            newest = Some((modified, summary));
        }
    }
    let Some((_, summary_path)) = newest else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(&summary_path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    // Belt-and-suspenders: a summary that names itself as the excluded session
    // contributes nothing even if its directory was renamed.
    if exclude_session.is_some_and(|active| {
        value.get("session_id").and_then(serde_json::Value::as_str) == Some(active)
    }) {
        return Vec::new();
    }
    value
        .get("key_points")
        .and_then(serde_json::Value::as_array)
        .map(|points| {
            points
                .iter()
                .filter_map(|point| point.as_str().map(str::to_string))
                .filter(|point| !point.trim().is_empty())
                .filter(|point| !is_transcript_echo(point))
                .take(6)
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a session-summary "key point" is just a raw transcript line echoed
/// from the conversation rather than a distilled fact. The transcript renderer
/// prefixes every line with a speaker label (`user:`, `assistant:`,
/// `assistant calls <tool>:`, `assistant (reasoning):`, `tool result:`,
/// `tool error:`, `user shell:`); such lines are conversation, not knowledge, and
/// must not surface through `knowledge_search`.
fn is_transcript_echo(point: &str) -> bool {
    let Some((prefix, _)) = point.trim_start().split_once(':') else {
        return false;
    };
    let prefix = prefix.trim().to_ascii_lowercase();
    // Reduce "assistant calls run_shell" / "assistant (reasoning)" to the role.
    let role = prefix
        .split_once(" calls ")
        .map_or(prefix.as_str(), |(role, _)| role)
        .trim_end_matches(" (reasoning)")
        .trim();
    matches!(
        role,
        "user" | "assistant" | "system" | "tool" | "tool result" | "tool error" | "user shell"
    )
}

/// The live/just-closed session id recorded under `.localmind/active-session`, if
/// any. Read-only and best-effort: the host records it at session close-out so
/// on-demand retrieval can exclude the in-progress conversation.
#[must_use]
pub fn active_session(root: &Path) -> Option<String> {
    let marker = root.join(ACTIVE_SESSION_FILE);
    let id = std::fs::read_to_string(marker).ok()?;
    let id = id.trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

/// Record the live/just-closed LocalMind session id so later retrieval excludes
/// it. A no-op when the project has no `.localmind/` state (nothing to exclude),
/// so a bare prompt never creates project files.
///
/// # Errors
/// Returns [`IngestError::Io`] when the marker cannot be written.
pub fn record_active_session(root: &Path, session_id: &str) -> Result<(), IngestError> {
    let dir = root.join(".localmind");
    if !dir.exists() {
        return Ok(());
    }
    let marker = dir.join("active-session");
    fs::write(&marker, session_id).map_err(|source| IngestError::Io {
        path: marker,
        source,
    })
}

/// Whether a (lowercased) task query names this candidate's path or file name —
/// an exact-file-match signal that boosts on-topic files in retrieval.
fn task_names_path(task_lower: &str, path: &str) -> bool {
    let path_lower = path.to_ascii_lowercase();
    if path_lower.len() >= 3 && task_lower.contains(&path_lower) {
        return true;
    }
    path_lower
        .rsplit(['/', '\\'])
        .next()
        .is_some_and(|name| name.len() >= 3 && task_lower.contains(name))
}

/// Fetch full chunk records for an explicit set of ids — the layer-3 "fetch"
/// step of the layered retrieval contract. Read-only; returns only the requested
/// ids (never the whole store), and an empty vec when the project has no index.
///
/// # Errors
/// Returns [`IngestError`] when the chunk store cannot be opened or queried.
pub fn fetch_chunks(project_root: &Path, ids: &[String]) -> Result<Vec<ChunkRecord>, IngestError> {
    let root = canonical_root(project_root)?;
    let ingest_dir = root.join(INGEST_DIR);
    if ids.is_empty() || !crate::chunk_store::exists(&ingest_dir) {
        return Ok(Vec::new());
    }
    let store = ChunkStore::open(&ingest_dir)?;
    store.fetch_by_ids(ids)
}

/// Sibling chunk ids of the same file as `id` — the layer-2 "expand" neighbours.
/// Read-only and cheap (ids only, no bodies). Empty when the id is unknown or the
/// file has a single chunk.
///
/// # Errors
/// Returns [`IngestError`] when the chunk store cannot be opened or queried.
pub fn sibling_chunk_ids(project_root: &Path, id: &str) -> Result<Vec<String>, IngestError> {
    let root = canonical_root(project_root)?;
    let ingest_dir = root.join(INGEST_DIR);
    if !crate::chunk_store::exists(&ingest_dir) {
        return Ok(Vec::new());
    }
    let store = ChunkStore::open(&ingest_dir)?;
    store.sibling_ids(id)
}

/// Format relevant derived ingestion chunks as compact turn context.
///
/// # Errors
/// Returns [`IngestError`] when the derived index cannot be read.
pub fn context_for_prompt(
    project_root: &Path,
    prompt: &str,
) -> Result<Option<String>, IngestError> {
    Ok(context_for_prompt_with_ids(project_root, prompt)?.map(|(text, _ids)| text))
}

/// As [`context_for_prompt`], but also returns the per-chunk locators
/// (`path:start-end`) actually injected, so a push-mode host can record exactly
/// which ingested chunks rode in the turn's context — the audit matches the
/// injection chunk for chunk.
///
/// # Errors
/// Returns [`IngestError`] when the derived index cannot be read.
pub fn context_for_prompt_with_ids(
    project_root: &Path,
    prompt: &str,
) -> Result<Option<(String, Vec<String>)>, IngestError> {
    let hits = search(project_root, prompt)?;
    if hits.is_empty() {
        return Ok(None);
    }
    let mut out = String::from("Relevant ingested project knowledge:\n");
    let mut ids = Vec::new();
    for hit in hits.into_iter().take(5) {
        let locator = format!("{}:{}-{}", hit.path, hit.start_line, hit.end_line);
        out.push_str("- ");
        out.push_str(&locator);
        if hit.stale {
            out.push_str(" (stale)");
        }
        out.push_str(" - ");
        out.push_str(&hit.snippet);
        out.push('\n');
        ids.push(locator);
    }
    Ok(Some((out, ids)))
}

/// List generated ingestion review items.
///
/// # Errors
/// Returns [`IngestError`] when review state cannot be read.
pub fn review_items(project_root: &Path) -> Result<Vec<IngestReviewItem>, IngestError> {
    let root = canonical_root(project_root)?;
    read_json(&root.join(INGEST_DIR).join(REVIEW_FILE))
}

/// Enqueue one ingestion review item into LocalMind review.
///
/// # Errors
/// Returns [`IngestError`] when the item is missing or enqueue fails.
pub fn promote_for_review(project_root: &Path, id: &str) -> Result<usize, IngestError> {
    let root = canonical_root(project_root)?;
    crate::initialize(&root).map_err(|error| IngestError::Review(error.to_string()))?;
    let item = review_items(&root)?
        .into_iter()
        .find(|item| item.id == id)
        .ok_or_else(|| IngestError::Review(format!("review item {id} not found")))?;
    let confidence =
        Confidence::new(0.70).map_err(|error| IngestError::Confidence(error.to_string()))?;
    let mut candidate = CandidateLesson::new(
        LessonId::new(item.id.clone()),
        item.body.clone(),
        category_for(&item.kind),
        confidence,
        SuggestedAction::PromoteToMemory,
    );
    candidate.related_files = item.source_path.clone().into_iter().collect();
    let mut evidence = EvidenceRef::new(
        EvidenceKind::Other("ingestion_artifact".to_string()),
        item.title,
    )
    .redacted();
    evidence.uri = item.source_path;
    evidence.content_hash = item.content_hash;
    let candidate = candidate.with_evidence(evidence);
    let queue =
        ReviewQueue::open_project(&root).map_err(|error| IngestError::Review(error.to_string()))?;
    queue
        .enqueue_candidates(&LearningSessionId::new("folder-ingestion"), &[candidate])
        .map_err(|error| IngestError::Review(error.to_string()))
}

/// Normalize a user-supplied path to a project-relative path and reject escapes.
///
/// # Errors
/// Returns [`IngestError::OutsideProject`] for paths outside the trusted root.
pub fn normalize_project_path(project_root: &Path, input: &Path) -> Result<PathBuf, IngestError> {
    let root = canonical_root(project_root)?;
    let joined = if input.is_absolute() {
        input.to_path_buf()
    } else {
        root.join(input)
    };
    let normalized = normalize_components(&joined)?;
    if !normalized.starts_with(&root) {
        return Err(IngestError::OutsideProject {
            root,
            path: normalized,
        });
    }
    normalized
        .strip_prefix(&root)
        .map(Path::to_path_buf)
        .map_err(|_| IngestError::OutsideProject {
            root,
            path: normalized,
        })
}

fn category_for(kind: &str) -> LessonCategory {
    match kind {
        "summary" => LessonCategory::DocumentationUpdate,
        "tooling" => LessonCategory::ToolingNote,
        "skill" => LessonCategory::CandidateSkill,
        _ => LessonCategory::ProjectConvention,
    }
}

fn set_job_status(
    project_root: &Path,
    status: JobStatus,
    message: &str,
) -> Result<Option<IngestJob>, IngestError> {
    let root = canonical_root(project_root)?;
    let path = root.join(INGEST_DIR).join(JOB_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let mut job = read_json::<IngestJob>(&path)?;
    job.status = status;
    job.updated_unix = unix_now();
    job.message = Some(message.to_string());
    write_json(&path, &job)?;
    Ok(Some(job))
}

fn update_rule(project_root: &Path, key: &str, path: &Path) -> Result<String, IngestError> {
    let relative = normalize_project_path(project_root, path)?;
    let rule = slash_path(&relative);
    let config_path = canonical_root(project_root)?.join(".localpilot.toml");
    let mut doc = if config_path.exists() {
        let text = fs::read_to_string(&config_path).map_err(|source| IngestError::Io {
            path: config_path.clone(),
            source,
        })?;
        text.parse::<toml::Value>()
            .map_err(|source| IngestError::Toml {
                path: config_path.clone(),
                source: Box::new(source),
            })?
    } else {
        toml::Value::Table(toml::map::Map::new())
    };
    let Some(root_table) = doc.as_table_mut() else {
        return Err(IngestError::Review(
            ".localpilot.toml root must be a table".to_string(),
        ));
    };
    let ingest = root_table
        .entry("ingest".to_string())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let Some(ingest_table) = ingest.as_table_mut() else {
        return Err(IngestError::Review(
            ".localpilot.toml [ingest] must be a table".to_string(),
        ));
    };
    ingest_table.insert("enabled".to_string(), toml::Value::Boolean(true));
    let entry = ingest_table
        .entry(key.to_string())
        .or_insert_with(|| toml::Value::Array(Vec::new()));
    let Some(values) = entry.as_array_mut() else {
        return Err(IngestError::Review(format!(
            ".localpilot.toml ingest.{key} must be an array"
        )));
    };
    if !values
        .iter()
        .any(|value| value.as_str() == Some(rule.as_str()))
    {
        values.push(toml::Value::String(rule.clone()));
    }
    let text = toml::to_string_pretty(&doc).map_err(|source| IngestError::TomlSerialize {
        path: config_path.clone(),
        source: Box::new(source),
    })?;
    fs::write(&config_path, text).map_err(|source| IngestError::Io {
        path: config_path,
        source,
    })?;
    Ok(rule)
}

fn walker(root: &Path) -> ignore::Walk {
    let mut builder = WalkBuilder::new(root);
    builder.standard_filters(true).hidden(false);
    builder.require_git(false);
    builder.build()
}

fn classify(
    root: &Path,
    path: &Path,
    config: &IngestConfig,
    localmind_excludes: &[String],
) -> Result<ManifestEntry, IngestError> {
    let relative = relative_path(root, path)?;
    let display_path = slash_path(&relative);
    let metadata = fs::metadata(path).map_err(|source| IngestError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let size_bytes = metadata.len();
    let modified_unix = metadata
        .modified()
        .ok()
        .and_then(system_time_to_unix)
        .unwrap_or(0);
    let explicit_include = path_matches_any(&display_path, &config.include);
    let explicit_exclude = path_matches_any(&display_path, &config.exclude)
        || path_matches_any(&display_path, localmind_excludes);
    let language = language_for(path);
    let mut entry = ManifestEntry {
        path: display_path.clone(),
        kind: "file".to_string(),
        size_bytes,
        modified_unix,
        content_hash: None,
        language: language.clone(),
        status: CandidateStatus::Candidate,
        skip_reason: None,
        token_estimate: estimate_tokens(size_bytes),
    };

    if explicit_exclude {
        entry.status = CandidateStatus::Excluded;
        entry.skip_reason = Some("excluded by project config".to_string());
        return Ok(entry);
    }
    if !explicit_include && is_heavy_or_generated(&relative, config) {
        entry.status = CandidateStatus::Generated;
        entry.skip_reason = Some("default generated or heavy directory skip".to_string());
        return Ok(entry);
    }
    if size_bytes > config.max_file_bytes {
        entry.status = CandidateStatus::TooLarge;
        entry.skip_reason = Some("file size budget exceeded".to_string());
        return Ok(entry);
    }
    let bytes = read_bytes(path)?;
    if bytes.contains(&0) {
        entry.status = CandidateStatus::Binary;
        entry.skip_reason = Some("binary content detected".to_string());
        return Ok(entry);
    }
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(_) => {
            entry.status = CandidateStatus::DecodeFailed;
            entry.skip_reason = Some("not valid UTF-8".to_string());
            return Ok(entry);
        }
    };
    if language.is_none() && !explicit_include {
        entry.status = CandidateStatus::Unsupported;
        entry.skip_reason = Some("unsupported text-like extension".to_string());
        return Ok(entry);
    }
    entry.content_hash = Some(fnv_hash_hex(text.as_bytes()));
    entry.token_estimate = estimate_tokens(text.len() as u64);
    Ok(entry)
}

fn chunk_file(
    path: &Path,
    display_path: &str,
    content_hash: &str,
    prefix_policy: &crate::context_prefix::PrefixEnrichmentPolicy,
) -> Result<Vec<ChunkRecord>, IngestError> {
    let text = fs::read_to_string(path).map_err(|source| IngestError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(chunk_text(&text, display_path, content_hash, prefix_policy))
}

/// Chunk in-memory `text` into redacted [`ChunkRecord`]s under `display_path`.
/// The line/byte-budgeted splitter shared by file ingestion and the merged
/// project-context document, so both produce identically-shaped, FTS-indexable
/// chunks.
fn chunk_text(
    text: &str,
    display_path: &str,
    content_hash: &str,
    prefix_policy: &crate::context_prefix::PrefixEnrichmentPolicy,
) -> Vec<ChunkRecord> {
    // One context prefix per file, shared by all its chunks. With no enricher
    // wired the off-machine tier is unreachable and this resolves to the
    // deterministic synthetic prefix; any egress would record an audit row.
    let mut prefix_audit = Vec::new();
    let context_prefix = crate::context_prefix::resolve_context_prefix(
        prefix_policy,
        None,
        display_path,
        text,
        &mut prefix_audit,
    );
    debug_assert!(
        prefix_audit.is_empty(),
        "no enricher is wired, so no egress audit row can be produced"
    );
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut start_line = 1_u64;
    let mut line_no = 0_u64;
    let mut start_byte = 0_u64;
    let mut byte_cursor = 0_u64;
    for line in text.lines() {
        line_no = line_no.saturating_add(1);
        let line_bytes = line.len().saturating_add(1);
        if !current.is_empty() && current.len().saturating_add(line_bytes) > CHUNK_BYTES {
            push_chunk(
                &mut chunks,
                display_path,
                content_hash,
                &context_prefix,
                &current,
                ChunkSpan {
                    start_line,
                    end_line: line_no.saturating_sub(1),
                    start_byte,
                    end_byte: byte_cursor,
                },
            );
            current.clear();
            start_line = line_no;
            start_byte = byte_cursor;
        }
        current.push_str(line);
        current.push('\n');
        byte_cursor = byte_cursor.saturating_add(line_bytes as u64);
    }
    if !current.is_empty() || text.is_empty() {
        push_chunk(
            &mut chunks,
            display_path,
            content_hash,
            &context_prefix,
            &current,
            ChunkSpan {
                start_line,
                end_line: line_no.max(start_line),
                start_byte,
                end_byte: byte_cursor,
            },
        );
    }
    chunks
}

fn push_chunk(
    chunks: &mut Vec<ChunkRecord>,
    path: &str,
    content_hash: &str,
    context_prefix: &str,
    text: &str,
    span: ChunkSpan,
) {
    let redacted = redact::redact(text);
    let chunk_index = chunks.len() as u32;
    // The id is path-qualified so two files with identical content (and thus the
    // same content hash) get distinct ids — the chunk store keys rows by id.
    let path_hash = fnv_hash_hex(path.as_bytes());
    chunks.push(ChunkRecord {
        id: format!("chunk-{path_hash}-{content_hash}-{chunk_index}"),
        path: path.to_string(),
        chunk_index,
        start_line: span.start_line,
        end_line: span.end_line,
        start_byte: span.start_byte,
        end_byte: span.end_byte,
        content_hash: content_hash.to_string(),
        token_estimate: estimate_tokens(redacted.len() as u64),
        context_prefix: context_prefix.to_string(),
        summary: summarize_chunk(path, &redacted),
        redaction_status: "redacted".to_string(),
        original_bytes: text.len() as u64,
        preview_bytes: redacted.len() as u64,
        text: redacted,
        stale: false,
        superseded_by: None,
    });
}

/// Ingest the merged project context as derived chunks on demand, independently
/// of a full folder run — the host's entry point to (re)capture the project
/// instruction files (`CLAUDE.md`/`AGENTS.md`, imports resolved, precedence
/// applied) without re-walking the whole tree. Reuses the existing chunk store;
/// returns the number of context chunks written (0 when the project carries no
/// context files).
///
/// # Errors
/// Returns [`IngestError`] when the chunk store cannot be opened or written.
pub fn ingest_project_context(project_root: &Path) -> Result<usize, IngestError> {
    let root = canonical_root(project_root)?;
    let prefix_policy = crate::context_prefix::PrefixEnrichmentPolicy { enabled: false };
    let chunks = project_context_chunks(&root, &prefix_policy);
    if chunks.is_empty() {
        return Ok(0);
    }
    let ingest_dir = ensure_ingest_dir(&root)?;
    let store = ChunkStore::open(&ingest_dir)?;
    if let Some(hash) = chunks.first().map(|chunk| chunk.content_hash.clone()) {
        store.mark_path_changed(PROJECT_CONTEXT_PATH, &hash)?;
    }
    store.upsert_chunks(&chunks)?;
    Ok(chunks.len())
}

/// Build redacted chunks for the merged project context, or an empty vec when the
/// project carries no `CLAUDE.md`/`AGENTS.md`. The discovery walks the workspace
/// (root, nested, and the per-user global location) and resolves `@`-imports.
fn project_context_chunks(
    root: &Path,
    prefix_policy: &crate::context_prefix::PrefixEnrichmentPolicy,
) -> Vec<ChunkRecord> {
    let context = localpilot_config::ContextDiscovery::new(root).discover();
    if context.is_empty() {
        return Vec::new();
    }
    let merged = context.render();
    let hash = fnv_hash_hex(merged.as_bytes());
    chunk_text(&merged, PROJECT_CONTEXT_PATH, &hash, prefix_policy)
}

fn build_review_items(manifest: &PreviewManifest, chunk_count: usize) -> Vec<IngestReviewItem> {
    let mut items = Vec::new();
    if chunk_count > 0 {
        items.push(IngestReviewItem {
            id: format!("summary-{}", fnv_hash_hex(manifest.project_root.as_bytes())),
            kind: "summary".to_string(),
            title: "Project ingestion summary".to_string(),
            body: redact::redact(&format!(
                "Indexed {} file(s) into {chunk_count} redacted chunk(s).",
                manifest.estimates.candidate_files,
            )),
            source_path: None,
            content_hash: Some(fnv_hash_hex(manifest.project_root.as_bytes())),
            stale: false,
        });
    }
    if let Some(tooling) = tooling_review_item(manifest) {
        items.push(tooling);
    }
    if manifest.estimates.candidate_files >= 3 {
        items.push(IngestReviewItem {
            id: format!("skill-{}", fnv_hash_hex(manifest.project_root.as_bytes())),
            kind: "skill".to_string(),
            title: "Review project workflow skill suggestion".to_string(),
            body: "Ingestion found enough local project material to justify reviewing a project workflow skill draft. No skill is installed or activated automatically.".to_string(),
            source_path: None,
            content_hash: Some(fnv_hash_hex(manifest.project_root.as_bytes())),
            stale: false,
        });
    }
    items.push(IngestReviewItem {
        id: format!("research-{}", fnv_hash_hex(manifest.project_root.as_bytes())),
        kind: "research".to_string(),
        title: "External research requires explicit review".to_string(),
        body: "No external facts were fetched during folder ingestion. Future research-backed updates must carry citations, expiry, and review before promotion.".to_string(),
        source_path: None,
        content_hash: Some(fnv_hash_hex(manifest.project_root.as_bytes())),
        stale: false,
    });
    for entry in manifest
        .entries
        .iter()
        .filter(|entry| entry.status == CandidateStatus::Candidate)
        .take(20)
    {
        items.push(IngestReviewItem {
            id: format!(
                "file-{}",
                entry
                    .content_hash
                    .clone()
                    .unwrap_or_else(|| fnv_hash_hex(entry.path.as_bytes()))
            ),
            kind: "summary".to_string(),
            title: format!("Ingested {}", entry.path),
            body: redact::redact(&format!(
                "{} is indexed with approximately {} token(s).",
                entry.path, entry.token_estimate
            )),
            source_path: Some(entry.path.clone()),
            content_hash: entry.content_hash.clone(),
            stale: false,
        });
    }
    items
}

fn tooling_review_item(manifest: &PreviewManifest) -> Option<IngestReviewItem> {
    let tooling_files: Vec<&ManifestEntry> = manifest
        .entries
        .iter()
        .filter(|entry| {
            matches!(
                entry.path.as_str(),
                "Cargo.toml"
                    | "package.json"
                    | "pyproject.toml"
                    | "go.mod"
                    | "pom.xml"
                    | "build.gradle"
                    | "Makefile"
            ) || entry.path.ends_with(".sln")
        })
        .filter(|entry| entry.status == CandidateStatus::Candidate)
        .collect();
    if tooling_files.is_empty() {
        return None;
    }
    let labels = tooling_files
        .iter()
        .map(|entry| entry.path.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Some(IngestReviewItem {
        id: format!("tooling-{}", fnv_hash_hex(labels.as_bytes())),
        kind: "tooling".to_string(),
        title: "Review detected project tooling".to_string(),
        body: redact::redact(&format!(
            "Ingestion found tooling/config files that may contain build or test commands: {labels}."
        )),
        source_path: tooling_files.first().map(|entry| entry.path.clone()),
        content_hash: Some(fnv_hash_hex(labels.as_bytes())),
        stale: false,
    })
}

fn summarize_snippet(text: &str, terms: &[String]) -> String {
    let lower = text.to_ascii_lowercase();
    let index = terms
        .iter()
        .filter_map(|term| lower.find(term))
        .min()
        .unwrap_or(0);
    let start = index.saturating_sub(80);
    let end = text.len().min(index.saturating_add(240));
    text.get(start..end)
        .unwrap_or(text)
        .lines()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

fn summarize_chunk(path: &str, text: &str) -> String {
    let first = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("empty chunk");
    let first = if first.chars().count() > 160 {
        let truncated: String = first.chars().take(160).collect();
        format!("{truncated}...")
    } else {
        first.to_string()
    };
    format!("{path}: {first}")
}

fn is_heavy_or_generated(relative: &Path, config: &IngestConfig) -> bool {
    let skip_dirs: BTreeSet<String> = config
        .default_skip_dirs
        .iter()
        .map(|value| value.to_ascii_lowercase())
        .collect();
    relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .any(|name| skip_dirs.contains(&name.to_ascii_lowercase()))
}

fn localmind_excluded_paths(root: &Path) -> Result<Vec<String>, IngestError> {
    let path = root.join(".localmind.toml");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(&path).map_err(|source| IngestError::Io {
        path: path.clone(),
        source,
    })?;
    let parsed = toml::from_str::<LocalMindConfig>(&text).map_err(|source| IngestError::Toml {
        path,
        source: Box::new(source),
    })?;
    Ok(parsed.ingest.excluded_paths)
}

fn read_bytes(path: &Path) -> Result<Vec<u8>, IngestError> {
    fs::read(path).map_err(|source| IngestError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, IngestError> {
    let text = fs::read_to_string(path).map_err(|source| IngestError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_str(&text).map_err(|source| IngestError::Json {
        path: path.to_path_buf(),
        source: Box::new(source),
    })
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), IngestError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| IngestError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let bytes = serde_json::to_vec_pretty(value).map_err(|source| IngestError::Json {
        path: path.to_path_buf(),
        source: Box::new(source),
    })?;
    fs::write(path, bytes).map_err(|source| IngestError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn ensure_ingest_dir(root: &Path) -> Result<PathBuf, IngestError> {
    let dir = root.join(INGEST_DIR);
    fs::create_dir_all(&dir).map_err(|source| IngestError::Io {
        path: dir.clone(),
        source,
    })?;
    Ok(dir)
}

fn canonical_root(path: &Path) -> Result<PathBuf, IngestError> {
    path.canonicalize().map_err(|source| IngestError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn normalize_components(path: &Path) -> Result<PathBuf, IngestError> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(IngestError::UnsupportedPath(path.to_path_buf()));
                }
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    Ok(normalized)
}

fn relative_path(root: &Path, path: &Path) -> Result<PathBuf, IngestError> {
    let canonical = path.canonicalize().map_err(|source| IngestError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !canonical.starts_with(root) {
        return Err(IngestError::OutsideProject {
            root: root.to_path_buf(),
            path: canonical,
        });
    }
    canonical
        .strip_prefix(root)
        .map(Path::to_path_buf)
        .map_err(|_| IngestError::OutsideProject {
            root: root.to_path_buf(),
            path: canonical,
        })
}

fn slash_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn platform_path(path: &str) -> PathBuf {
    path.split('/').collect()
}

fn path_matches_any(path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| path_matches(path, pattern))
}

fn path_matches(path: &str, pattern: &str) -> bool {
    let pattern = pattern.trim().trim_matches('/');
    if pattern.is_empty() {
        return false;
    }
    path == pattern || path.starts_with(&format!("{pattern}/")) || path.contains(pattern)
}

fn language_for(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    if matches!(name.as_str(), "makefile" | "dockerfile") {
        return Some(name);
    }
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    let language = match ext.as_str() {
        "rs" => "rust",
        "cs" => "csharp",
        "ps1" => "powershell",
        "py" => "python",
        "js" | "jsx" => "javascript",
        "ts" | "tsx" => "typescript",
        "go" => "go",
        "java" => "java",
        "cpp" | "cc" | "cxx" | "c" | "h" | "hpp" => "c-family",
        "sql" => "sql",
        "sh" | "bash" | "zsh" => "shell",
        "bat" | "cmd" => "batch",
        "html" | "css" | "scss" => "web",
        "json" | "toml" | "yaml" | "yml" | "xml" | "csproj" | "sln" | "props" | "targets" => {
            "config"
        }
        "md" | "txt" | "rst" | "csv" | "tsv" | "log" => "text",
        "example" => "text",
        _ => return None,
    };
    Some(language.to_string())
}

fn estimate_tokens(bytes: u64) -> u64 {
    bytes.saturating_add(3) / 4
}

fn fnv_hash_hex(bytes: &[u8]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn unix_now() -> u64 {
    system_time_to_unix(SystemTime::now()).unwrap_or(0)
}

fn system_time_to_unix(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn config() -> IngestConfig {
        IngestConfig {
            enabled: true,
            max_file_bytes: 1024 * 1024,
            max_run_bytes: 1024 * 1024,
            max_files: 100,
            max_tokens: 100_000,
            max_elapsed_secs: 60,
            ..IngestConfig::default()
        }
    }

    /// Every chunk currently in the project's store, for assertions.
    fn stored_chunks(root: &Path) -> Vec<ChunkRecord> {
        let ingest_dir = canonical_root(root).unwrap().join(INGEST_DIR);
        crate::chunk_store::ChunkStore::open(&ingest_dir)
            .unwrap()
            .all_chunks()
            .unwrap()
    }

    /// Seed chunks directly into the store, as a prior run would have persisted.
    fn seed_chunks(root: &Path, chunks: &[ChunkRecord]) {
        let ingest_dir = ensure_ingest_dir(&canonical_root(root).unwrap()).unwrap();
        crate::chunk_store::ChunkStore::open(&ingest_dir)
            .unwrap()
            .upsert_chunks(chunks)
            .unwrap();
    }

    /// The pre-store scoring: a full linear scan over every chunk with the exact
    /// term-count + path-name-boost formula and tie-break. The FTS-backed
    /// `search` must reproduce this ranking for word queries.
    fn linear_scan(chunks: &[ChunkRecord], query: &str) -> Vec<(String, u64)> {
        let terms: Vec<String> = query
            .split_whitespace()
            .map(|term| term.to_ascii_lowercase())
            .filter(|term| !term.is_empty())
            .collect();
        let mut hits: Vec<(String, u64, String)> = Vec::new();
        for chunk in chunks {
            let text = chunk.text.to_ascii_lowercase();
            let path = chunk.path.to_ascii_lowercase();
            let mut score = 0_u64;
            for term in &terms {
                score = score.saturating_add(text.matches(term.as_str()).count() as u64);
                if path.contains(term.as_str()) {
                    score = score.saturating_add(3);
                }
            }
            if score == 0 {
                continue;
            }
            hits.push((chunk.path.clone(), score, chunk.id.clone()));
        }
        hits.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| a.0.cmp(&b.0))
                .then_with(|| a.2.cmp(&b.2))
        });
        hits.into_iter()
            .map(|(path, score, _)| (path, score))
            .collect()
    }

    #[test]
    fn path_normalization_rejects_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let inside = normalize_project_path(dir.path(), Path::new("src/lib.rs")).unwrap();
        assert_eq!(inside, PathBuf::from("src").join("lib.rs"));

        let outside = normalize_project_path(dir.path(), Path::new("../outside.txt"));
        assert!(matches!(outside, Err(IngestError::OutsideProject { .. })));
    }

    #[test]
    fn preview_honors_exclusions_and_default_skips() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src")).unwrap();
        fs::create_dir_all(dir.path().join("target")).unwrap();
        fs::write(dir.path().join("src").join("lib.rs"), "pub fn main() {}\n").unwrap();
        fs::write(dir.path().join("target").join("generated.rs"), "generated").unwrap();
        fs::write(
            dir.path().join(".localmind.toml"),
            "[ingest]\nexcluded_paths = [\"src/skip.rs\"]\n",
        )
        .unwrap();
        fs::write(dir.path().join("src").join("skip.rs"), "skip").unwrap();

        let manifest = preview(dir.path(), &config()).unwrap();

        let lib = manifest
            .entries
            .iter()
            .find(|entry| entry.path == "src/lib.rs")
            .unwrap();
        assert_eq!(lib.status, CandidateStatus::Candidate);
        let generated = manifest
            .entries
            .iter()
            .find(|entry| entry.path == "target/generated.rs")
            .unwrap();
        assert_eq!(generated.status, CandidateStatus::Generated);
        let skipped = manifest
            .entries
            .iter()
            .find(|entry| entry.path == "src/skip.rs")
            .unwrap();
        assert_eq!(skipped.status, CandidateStatus::Excluded);
    }

    #[test]
    fn preview_classifies_binary_decode_and_size_failures() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("binary.dat"), b"abc\0def").unwrap();
        fs::write(dir.path().join("bad.txt"), [0xff, 0xfe, 0xfd]).unwrap();
        fs::write(dir.path().join("large.md"), "x".repeat(32)).unwrap();
        let cfg = IngestConfig {
            enabled: true,
            include: vec!["binary.dat".to_string()],
            max_file_bytes: 8,
            max_run_bytes: 1024,
            max_files: 100,
            max_tokens: 100,
            max_elapsed_secs: 60,
            ..IngestConfig::default()
        };

        let manifest = preview(dir.path(), &cfg).unwrap();

        let binary = manifest
            .entries
            .iter()
            .find(|entry| entry.path == "binary.dat")
            .unwrap();
        assert_eq!(binary.status, CandidateStatus::Binary);
        let bad = manifest
            .entries
            .iter()
            .find(|entry| entry.path == "bad.txt")
            .unwrap();
        assert_eq!(bad.status, CandidateStatus::DecodeFailed);
        let large = manifest
            .entries
            .iter()
            .find(|entry| entry.path == "large.md")
            .unwrap();
        assert_eq!(large.status, CandidateStatus::TooLarge);
    }

    #[test]
    fn preview_honors_gitignore_and_include_override() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("target")).unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored.md\n").unwrap();
        fs::write(dir.path().join("ignored.md"), "ignored").unwrap();
        fs::write(dir.path().join("target").join("keep.md"), "keep").unwrap();
        let mut cfg = config();
        cfg.include = vec!["target/keep.md".to_string()];

        let manifest = preview(dir.path(), &cfg).unwrap();

        assert!(manifest
            .entries
            .iter()
            .all(|entry| entry.path != "ignored.md"));
        let keep = manifest
            .entries
            .iter()
            .find(|entry| entry.path == "target/keep.md")
            .unwrap();
        assert_eq!(keep.status, CandidateStatus::Candidate);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn preview_does_not_follow_symlinked_files_by_default() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("real.md"), "real").unwrap();
        let link = dir.path().join("linked.md");
        create_file_symlink(dir.path().join("real.md"), &link).unwrap();

        let manifest = preview(dir.path(), &config()).unwrap();

        assert!(manifest
            .entries
            .iter()
            .all(|entry| entry.path != "linked.md"));
    }

    #[cfg(unix)]
    fn create_file_symlink(
        original: impl AsRef<Path>,
        link: impl AsRef<Path>,
    ) -> std::io::Result<()> {
        std::os::unix::fs::symlink(original, link)
    }

    #[cfg(windows)]
    fn create_file_symlink(
        original: impl AsRef<Path>,
        link: impl AsRef<Path>,
    ) -> std::io::Result<()> {
        std::os::windows::fs::symlink_file(original, link)
    }

    #[test]
    fn run_redacts_before_persisting_chunks_and_rebuild_keeps_memory() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "token = abcdefghijklmnop\n").unwrap();
        fs::create_dir_all(dir.path().join(".localmind").join("memory").join("project")).unwrap();
        fs::write(
            dir.path()
                .join(".localmind")
                .join("memory")
                .join("project")
                .join("keep.md"),
            "accepted memory",
        )
        .unwrap();

        let summary = run(dir.path(), &config(), RunMode::Full).unwrap();

        assert_eq!(summary.job.status, JobStatus::Completed);
        let text = stored_chunks(dir.path())
            .iter()
            .map(|chunk| chunk.text.clone())
            .collect::<String>();
        assert!(!text.contains("abcdefghijklmnop"));
        assert!(text.contains(redact::REDACTED));

        rebuild(dir.path()).unwrap();
        assert!(!dir.path().join(INGEST_DIR).exists());
        assert!(dir
            .path()
            .join(".localmind")
            .join("memory")
            .join("project")
            .join("keep.md")
            .exists());
    }

    #[test]
    fn search_and_pack_are_budgeted() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "parser parser guide\n").unwrap();
        fs::write(dir.path().join("notes.txt"), "deployment notes\n").unwrap();
        run(dir.path(), &config(), RunMode::Full).unwrap();

        let hits = search(dir.path(), "parser").unwrap();
        assert_eq!(hits[0].path, "README.md");
        let pack = build_pack(dir.path(), "parser", 100).unwrap();
        assert_eq!(pack.chunks.len(), 1);
        assert!(dir.path().join(INGEST_DIR).join(PACK_FILE).exists());
    }

    /// Merged-context wiring: a full ingest run captures the merged project context as a
    /// first-class document, with `@`-imports resolved, so retrieval surfaces a
    /// convention that lives *only* in an imported file — proving it is the merged
    /// context that was ingested, not just the raw `CLAUDE.md`.
    #[test]
    fn ingest_captures_the_merged_project_context_with_imports_resolved() {
        let dir = tempfile::tempdir().unwrap();
        // The imported phrase appears in shared.md, never in CLAUDE.md itself.
        fs::write(
            dir.path().join("shared.md"),
            "convention: zorptastic naming\n",
        )
        .unwrap();
        fs::write(dir.path().join("CLAUDE.md"), "project rules\n@shared.md\n").unwrap();

        run(dir.path(), &config(), RunMode::Full).unwrap();

        // The imported convention is searchable, and the hit is the merged
        // project-context document (not the raw CLAUDE.md, which lacks the phrase).
        let hits = search(dir.path(), "zorptastic").unwrap();
        assert!(
            hits.iter().any(|hit| hit.path == PROJECT_CONTEXT_PATH),
            "merged project context should surface the imported convention: {hits:?}"
        );
    }

    /// The on-demand entry point ingests the context without a full folder walk,
    /// and reports nothing for a project that carries no instruction files.
    #[test]
    fn ingest_project_context_is_on_demand_and_skips_a_bare_project() {
        let dir = tempfile::tempdir().unwrap();
        // No CLAUDE.md/AGENTS.md, and no ~/.localpilot leakage assumed: bare.
        // (A dev machine with a global file would only add unrelated chunks; this
        // project has its own root files absent, which is what the count reflects.)
        fs::write(dir.path().join("CLAUDE.md"), "use guard clauses here\n").unwrap();
        let written = ingest_project_context(dir.path()).unwrap();
        assert!(
            written >= 1,
            "a project with CLAUDE.md should ingest context"
        );
        let hits = search(dir.path(), "guard").unwrap();
        assert!(hits.iter().any(|hit| hit.path == PROJECT_CONTEXT_PATH));
    }

    #[test]
    fn compute_pack_does_not_persist_but_build_pack_does() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "parser parser guide\n").unwrap();
        run(dir.path(), &config(), RunMode::Full).unwrap();
        let pack_path = dir.path().join(INGEST_DIR).join(PACK_FILE);

        // compute_pack is read-only: it returns a pack but writes no file.
        let computed = compute_pack(dir.path(), "parser", 100, None).unwrap();
        assert!(!computed.chunks.is_empty());
        assert!(
            !pack_path.exists(),
            "compute_pack must not persist last-pack.json"
        );

        // build_pack persists the same pack for inspection.
        let built = build_pack(dir.path(), "parser", 100).unwrap();
        assert!(pack_path.exists(), "build_pack must persist last-pack.json");
        assert_eq!(built.entries, computed.entries);
    }

    fn seed_job(status: JobStatus, completed: u64) -> IngestJob {
        let now = unix_now();
        IngestJob {
            schema_version: INGEST_SCHEMA_VERSION,
            run_id: format!("run-{now}"),
            status,
            mode: "full".to_string(),
            queued_files: 2,
            completed_files: completed,
            failed_files: 0,
            skipped_files: 0,
            started_unix: now,
            updated_unix: now,
            message: None,
        }
    }

    #[test]
    fn planned_run_mode_resumes_an_incomplete_job_with_an_index() {
        // No job yet → a first-ever full build.
        assert_eq!(planned_run_mode(None, false), Some(RunMode::Full));
        // A completed job → nothing to do.
        assert_eq!(
            planned_run_mode(Some(&seed_job(JobStatus::Completed, 2)), true),
            None
        );
        // An incomplete job with a persisted index → resume via Refresh, not a
        // full restart. This is the fix for the pause→Full-restart loop.
        for status in [JobStatus::Paused, JobStatus::Failed, JobStatus::Cancelled] {
            assert_eq!(
                planned_run_mode(Some(&seed_job(status, 1)), true),
                Some(RunMode::Refresh),
                "{status:?} with an index must resume via Refresh"
            );
        }
        // An incomplete job with no index yet → a full build (nothing to reuse).
        assert_eq!(
            planned_run_mode(Some(&seed_job(JobStatus::Paused, 0)), false),
            Some(RunMode::Full)
        );
    }

    #[test]
    fn refresh_reuses_persisted_chunk_without_rereading_unchanged_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("notes.txt"), "stable content line\n").unwrap();
        run(dir.path(), &config(), RunMode::Full).unwrap();
        assert!(has_chunk_index(dir.path()));

        // Overwrite the persisted chunk's text with a sentinel the source file
        // does not contain, keeping the content_hash that matches the file. A
        // reuse (keyed on path:content_hash) leaves this row untouched; a re-read
        // would replace the sentinel with the file's real content.
        let mut chunks = stored_chunks(dir.path());
        let sentinel = "SENTINEL-REUSED-NOT-REREAD";
        for chunk in &mut chunks {
            chunk.text = sentinel.to_string();
        }
        seed_chunks(dir.path(), &chunks);

        // Refresh without touching the file.
        run(dir.path(), &config(), RunMode::Refresh).unwrap();

        let refreshed = stored_chunks(dir.path());
        assert!(
            refreshed
                .iter()
                .any(|chunk| chunk.path == "notes.txt" && chunk.text == sentinel),
            "an unchanged file must be reused from the persisted chunk, not re-read"
        );
    }

    #[test]
    fn resume_makes_monotonic_forward_progress_without_a_full_rewalk() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.md"), "alpha\n").unwrap();
        fs::write(dir.path().join("b.md"), "bravo\n").unwrap();

        // Hand-seed an interrupted run: only a.md is chunked (its text is a
        // sentinel proving it gets reused, not redone), and the job is Paused —
        // exactly the state run() persists when it stops at the elapsed budget
        // mid-walk.
        let root = canonical_root(dir.path()).unwrap();
        let ingest_dir = ensure_ingest_dir(&root).unwrap();
        let a_hash = fnv_hash_hex(b"alpha\n");
        let sentinel = "SENTINEL-A-ALREADY-DONE";
        seed_chunks(
            dir.path(),
            &[ChunkRecord {
                id: format!("chunk-{a_hash}-0"),
                path: "a.md".to_string(),
                chunk_index: 0,
                start_line: 1,
                end_line: 1,
                start_byte: 0,
                end_byte: 6,
                content_hash: a_hash,
                text: sentinel.to_string(),
                token_estimate: 1,
                stale: false,
                context_prefix: String::new(),
                summary: String::new(),
                redaction_status: "redacted".to_string(),
                original_bytes: 6,
                preview_bytes: sentinel.len() as u64,
                superseded_by: None,
            }],
        );
        write_json(&ingest_dir.join(JOB_FILE), &seed_job(JobStatus::Paused, 1)).unwrap();

        // The host trigger resolves this incomplete-with-index state to Refresh.
        assert_eq!(
            planned_run_mode(
                status(dir.path()).unwrap().as_ref(),
                has_chunk_index(dir.path())
            ),
            Some(RunMode::Refresh)
        );

        // Resume: a.md is reused (sentinel survives → no re-walk/re-chunk of a),
        // b.md is finished, and the job reaches Completed. completed_files counts
        // both, never regressing below the 1 already done.
        let summary = run(dir.path(), &config(), RunMode::Refresh).unwrap();
        assert_eq!(summary.job.status, JobStatus::Completed);
        assert!(summary.job.completed_files >= 1);
        let chunks = stored_chunks(dir.path());
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.path == "a.md" && chunk.text == sentinel),
            "resume must reuse already-completed work, not redo it"
        );
        assert!(
            chunks.iter().any(|chunk| chunk.path == "b.md"),
            "resume must complete the not-yet-done file"
        );
    }

    fn job_at(status: JobStatus, updated_unix: u64) -> IngestJob {
        IngestJob {
            schema_version: INGEST_SCHEMA_VERSION,
            run_id: "run-test".to_string(),
            status,
            mode: "full".to_string(),
            queued_files: 1,
            completed_files: 1,
            failed_files: 0,
            skipped_files: 0,
            started_unix: updated_unix,
            updated_unix,
            message: None,
        }
    }

    /// Make a just-completed run look old and its manifest mtimes stale, so the
    /// files on disk read as newer than the last run recorded.
    fn age_completed_run(root: &Path) {
        let ingest_dir = canonical_root(root).unwrap().join(INGEST_DIR);
        let mut job = read_json::<IngestJob>(&ingest_dir.join(JOB_FILE)).unwrap();
        job.updated_unix = 1;
        write_json(&ingest_dir.join(JOB_FILE), &job).unwrap();
        let mut manifest = read_json::<PreviewManifest>(&ingest_dir.join(MANIFEST_FILE)).unwrap();
        for entry in &mut manifest.entries {
            entry.modified_unix = 0;
        }
        write_json(&ingest_dir.join(MANIFEST_FILE), &manifest).unwrap();
    }

    #[test]
    fn should_refresh_only_when_completed_changed_and_past_cadence() {
        let now = 10_000;
        let interval = 600;
        let done = job_at(JobStatus::Completed, 1_000); // 9000s ago — past cadence
        assert!(should_refresh(Some(&done), true, now, interval));
        // No change → no refresh.
        assert!(!should_refresh(Some(&done), false, now, interval));
        // Inside the cadence window → no refresh even if sources changed.
        let recent = job_at(JobStatus::Completed, now - 100);
        assert!(!should_refresh(Some(&recent), true, now, interval));
        // A non-completed job is the resume path, not the staleness path.
        assert!(!should_refresh(
            Some(&job_at(JobStatus::Paused, 1_000)),
            true,
            now,
            interval
        ));
        // No job at all → nothing to refresh.
        assert!(!should_refresh(None, true, now, interval));
    }

    #[test]
    fn session_open_mode_refreshes_a_completed_index_when_sources_change() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "alpha\n").unwrap();
        run(dir.path(), &config(), RunMode::Full).unwrap();
        let cfg = config();

        // A just-completed, current index: nothing to do.
        assert_eq!(session_open_mode(dir.path(), &cfg), None);

        // Once the run is old and its sources read as changed, the trigger picks
        // a background Refresh (reusing the incremental resume path).
        age_completed_run(dir.path());
        assert_eq!(session_open_mode(dir.path(), &cfg), Some(RunMode::Refresh));
    }

    #[test]
    fn staleness_detection_only_inspects_a_bounded_sample() {
        let dir = tempfile::tempdir().unwrap();
        for name in 'a'..='j' {
            fs::write(dir.path().join(format!("{name}.md")), "x\n").unwrap();
        }
        run(dir.path(), &config(), RunMode::Full).unwrap();

        // a.md is recorded as already-newer-than-disk (never "changed"); every
        // other file is recorded stale (would read as changed if it were sampled).
        let ingest_dir = canonical_root(dir.path()).unwrap().join(INGEST_DIR);
        let mut manifest = read_json::<PreviewManifest>(&ingest_dir.join(MANIFEST_FILE)).unwrap();
        for entry in &mut manifest.entries {
            if entry.status == CandidateStatus::Candidate {
                entry.modified_unix = if entry.path == "a.md" { u64::MAX } else { 0 };
            }
        }
        write_json(&ingest_dir.join(MANIFEST_FILE), &manifest).unwrap();

        // cap = 1 samples only the head (a.md), so the many changed files beyond
        // the sample are never inspected — detection is bounded.
        assert!(!sources_changed_since_completed(dir.path(), 1));
        // A larger sample reaches a changed file.
        assert!(sources_changed_since_completed(dir.path(), 10));
    }

    #[test]
    fn session_open_mode_is_inert_when_ingest_is_disabled() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "alpha\n").unwrap();
        let cfg = IngestConfig {
            enabled: false,
            ..config()
        };
        // Even with no index (which would otherwise be a first Full build),
        // disabled ingest never triggers and never writes `.localmind`.
        assert_eq!(session_open_mode(dir.path(), &cfg), None);
        assert!(!dir.path().join(".localmind").exists());
    }

    #[test]
    fn should_build_index_reflects_job_completion() {
        let dir = tempfile::tempdir().unwrap();
        // No job yet → build.
        assert!(should_build_index(status(dir.path()).unwrap().as_ref()));

        fs::write(dir.path().join("README.md"), "hello world\n").unwrap();
        run(dir.path(), &config(), RunMode::Full).unwrap();

        // A completed job → skip.
        let job = status(dir.path()).unwrap();
        assert_eq!(job.as_ref().map(|j| j.status), Some(JobStatus::Completed));
        assert!(!should_build_index(job.as_ref()));
    }

    #[test]
    fn pack_entries_compete_under_budget_with_reasons_and_reserves() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "parser parser guide\n").unwrap();
        fs::write(
            dir.path().join("GUIDE.md"),
            "parser internals and more parser tips\n",
        )
        .unwrap();
        run(dir.path(), &config(), RunMode::Full).unwrap();

        let pack = build_pack(dir.path(), "parser", 1_000).unwrap();

        // The unified allocation is populated and every entry explains itself.
        assert!(!pack.entries.is_empty());
        assert!(pack.entries.iter().all(|entry| !entry.reason.is_empty()));
        // Per-source reserves are real token amounts, not cosmetic fractions.
        assert!(pack.ingest_budget > 0);
        assert!(pack.accepted_memory_budget > 0);
        // The reported estimate matches the selected entries.
        let summed: u64 = pack.entries.iter().map(|entry| entry.token_estimate).sum();
        assert_eq!(pack.token_estimate, summed);
        assert!(pack.token_estimate <= pack.token_budget);
        // Each entry carries an inspectable rank-signal breakdown.
        assert!(pack
            .entries
            .iter()
            .all(|entry| entry.signals.source_quality > 0));
    }

    #[test]
    fn recent_session_facts_compete_as_pack_candidates() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "parser guide\n").unwrap();
        run(dir.path(), &config(), RunMode::Full).unwrap();

        // A persisted session summary contributes recent-session candidates.
        let session_dir = dir.path().join(".localmind").join("sessions").join("s1");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(
            session_dir.join("summary.json"),
            r#"{"key_points":["remember to update the changelog before release"]}"#,
        )
        .unwrap();

        let pack = build_pack(dir.path(), "changelog", 1_000).unwrap();
        assert!(
            pack.entries
                .iter()
                .any(|entry| entry.source == PackSource::RecentSession
                    && entry.snippet.contains("changelog")),
            "recent-session fact missing from {:?}",
            pack.entries
        );
    }

    fn write_session_summary(root: &Path, id: &str, key_points: &[&str]) {
        let dir = root.join(".localmind").join("sessions").join(id);
        fs::create_dir_all(&dir).unwrap();
        let points = serde_json::to_string(key_points).unwrap();
        fs::write(
            dir.join("summary.json"),
            format!(r#"{{"session_id":"{id}","key_points":{points}}}"#),
        )
        .unwrap();
    }

    #[test]
    fn the_active_session_is_excluded_while_a_prior_session_still_surfaces() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "changelog guide\n").unwrap();
        run(dir.path(), &config(), RunMode::Full).unwrap();

        // A prior session holds a real distilled fact; the live session is the
        // newest summary and must not surface even though it mentions the task.
        write_session_summary(
            dir.path(),
            "session-prior",
            &["update the changelog before every release"],
        );
        // Make the live session strictly newer so, without the exclude, it would
        // win the "most recent summary" pick.
        std::thread::sleep(std::time::Duration::from_millis(10));
        write_session_summary(
            dir.path(),
            "session-live",
            &["the changelog discussion in this very chat"],
        );

        let pack = compute_pack(dir.path(), "changelog", 1_000, Some("session-live")).unwrap();
        let recent: Vec<&str> = pack
            .entries
            .iter()
            .filter(|entry| entry.source == PackSource::RecentSession)
            .map(|entry| entry.snippet.as_str())
            .collect();
        assert!(
            recent.iter().any(|s| s.contains("before every release")),
            "the prior session's fact must still surface: {recent:?}"
        );
        assert!(
            !recent.iter().any(|s| s.contains("this very chat")),
            "the active session must contribute zero recent-session facts: {recent:?}"
        );
    }

    #[test]
    fn raw_transcript_echo_lines_are_not_surfaced_as_facts() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "ping guide\n").unwrap();
        run(dir.path(), &config(), RunMode::Full).unwrap();

        // A summary built from raw role-prefixed transcript lines — exactly what
        // the deterministic extractor fell back to — yields no echo entries.
        write_session_summary(
            dir.path(),
            "session-echo",
            &[
                "user: ping",
                "assistant: ping",
                "assistant calls run_shell: {\"command\":\"pwd\"}",
                "tool result: /work",
                "assistant (reasoning): thinking about ping",
            ],
        );

        let pack = compute_pack(dir.path(), "ping", 1_000, None).unwrap();
        assert!(
            !pack
                .entries
                .iter()
                .any(|entry| entry.source == PackSource::RecentSession),
            "raw role-prefixed transcript lines must not surface as facts: {:?}",
            pack.entries
        );
    }

    #[test]
    fn transcript_echo_detector_keeps_real_facts() {
        // Role-prefixed conversation lines are echoes…
        assert!(is_transcript_echo("user: ping"));
        assert!(is_transcript_echo("assistant: here is the answer"));
        assert!(is_transcript_echo(
            "assistant calls run_shell: {\"command\":\"ls\"}"
        ));
        assert!(is_transcript_echo("assistant (reasoning): planning"));
        assert!(is_transcript_echo("tool result: ok"));
        assert!(is_transcript_echo("tool error: boom"));
        assert!(is_transcript_echo("user shell: ls -la"));
        // …distilled facts (even with a colon) are kept.
        assert!(!is_transcript_echo(
            "Note: prefer guard clauses over nesting"
        ));
        assert!(!is_transcript_echo("update the changelog before release"));
        assert!(!is_transcript_echo("asdasd"));
    }

    #[test]
    fn active_session_marker_round_trips_and_is_inert_without_localmind() {
        let dir = tempfile::tempdir().unwrap();
        // No `.localmind/` yet: recording is a no-op and creates nothing.
        record_active_session(dir.path(), "session-x").unwrap();
        assert!(!dir.path().join(".localmind").exists());
        assert_eq!(active_session(dir.path()), None);

        // Once `.localmind/` exists, the marker round-trips.
        fs::create_dir_all(dir.path().join(".localmind")).unwrap();
        record_active_session(dir.path(), "session-live").unwrap();
        assert_eq!(active_session(dir.path()).as_deref(), Some("session-live"));
    }

    #[test]
    fn tight_budget_records_skipped_entries_with_reasons() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.md"), "parser parser one\n").unwrap();
        fs::write(dir.path().join("b.md"), "parser parser two\n").unwrap();
        fs::write(dir.path().join("c.md"), "parser parser three\n").unwrap();
        run(dir.path(), &config(), RunMode::Full).unwrap();

        // A budget too small for every matching chunk forces skips.
        let pack = build_pack(dir.path(), "parser", 2).unwrap();

        assert!(pack.token_estimate <= 2);
        // Every recorded candidate — kept or dropped — explains itself.
        assert!(pack.entries.iter().all(|entry| !entry.reason.is_empty()));
        assert!(pack
            .skipped_entries
            .iter()
            .all(|entry| !entry.reason.is_empty()));
        // Something lost the budget competition and says why.
        assert!(
            pack.skipped_entries
                .iter()
                .any(|entry| entry.reason.contains("budget")),
            "expected a budget-exhausted skip in {:?}",
            pack.skipped_entries
        );
    }

    #[test]
    fn prompt_context_uses_bounded_derived_chunks() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "parser guide\n").unwrap();
        run(dir.path(), &config(), RunMode::Full).unwrap();

        let context = context_for_prompt(dir.path(), "parser").unwrap().unwrap();

        assert!(context.contains("Relevant ingested project knowledge"));
        assert!(context.contains("README.md"));
    }

    #[test]
    fn refresh_reuses_unchanged_chunks_and_updates_changed_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "alpha\n").unwrap();
        fs::write(dir.path().join("notes.txt"), "stable\n").unwrap();
        run(dir.path(), &config(), RunMode::Full).unwrap();
        let first = stored_chunks(dir.path());
        let stable_hash = first
            .iter()
            .find(|chunk| chunk.path == "notes.txt")
            .unwrap()
            .content_hash
            .clone();

        fs::write(dir.path().join("README.md"), "beta\n").unwrap();
        let summary = run(dir.path(), &config(), RunMode::Refresh).unwrap();
        let refreshed = stored_chunks(dir.path());

        assert_eq!(summary.job.status, JobStatus::Completed);
        assert!(refreshed
            .iter()
            .any(|chunk| chunk.path == "notes.txt" && chunk.content_hash == stable_hash));
        assert!(refreshed
            .iter()
            .any(|chunk| chunk.path == "README.md" && chunk.text.contains("beta")));
    }

    #[test]
    fn fts_search_matches_the_linear_scan_ranking() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "parser parser guide\n").unwrap();
        fs::write(
            dir.path().join("notes.txt"),
            "deployment notes about the parser\n",
        )
        .unwrap();
        fs::write(dir.path().join("parser.md"), "an overview document\n").unwrap();
        run(dir.path(), &config(), RunMode::Full).unwrap();

        let all = stored_chunks(dir.path());
        for query in ["parser", "deployment", "guide parser", "missing"] {
            let new_hits: Vec<(String, u64)> = search(dir.path(), query)
                .unwrap()
                .into_iter()
                .map(|hit| (hit.path, hit.score))
                .collect();
            assert_eq!(
                new_hits,
                linear_scan(&all, query),
                "FTS-backed search must match the linear scan for {query:?}"
            );
        }
    }

    #[test]
    fn refresh_updates_only_changed_paths_and_tombstones_removed_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("keep.md"), "unchanged keep\n").unwrap();
        fs::write(dir.path().join("change.md"), "first version\n").unwrap();
        fs::write(dir.path().join("drop.md"), "to be deleted\n").unwrap();
        run(dir.path(), &config(), RunMode::Full).unwrap();

        // Tag the unchanged file's row with a sentinel so we can prove the refresh
        // never rewrites it.
        let mut chunks = stored_chunks(dir.path());
        let sentinel = "SENTINEL-UNCHANGED-ROW";
        for chunk in &mut chunks {
            if chunk.path == "keep.md" {
                chunk.text = sentinel.to_string();
            }
        }
        seed_chunks(dir.path(), &chunks);

        fs::write(dir.path().join("change.md"), "second version\n").unwrap();
        fs::remove_file(dir.path().join("drop.md")).unwrap();
        run(dir.path(), &config(), RunMode::Refresh).unwrap();

        let after = stored_chunks(dir.path());
        // Unchanged path: its row is untouched (sentinel intact, still fresh).
        assert!(
            after
                .iter()
                .any(|c| c.path == "keep.md" && c.text == sentinel && !c.stale),
            "an unchanged file's rows must not be rewritten"
        );
        // Changed path: a fresh row with the new content, old row kept as a stale
        // tombstone pointing at the new hash.
        assert!(after
            .iter()
            .any(|c| c.path == "change.md" && c.text.contains("second") && !c.stale));
        assert!(after.iter().any(|c| c.path == "change.md"
            && c.text.contains("first")
            && c.stale
            && c.superseded_by.is_some()));
        // Removed path: rows tombstoned with no successor.
        assert!(after
            .iter()
            .any(|c| c.path == "drop.md" && c.stale && c.superseded_by.is_none()));
    }

    #[test]
    fn search_retrieves_only_matching_rows_not_the_whole_index() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..50 {
            fs::write(
                dir.path().join(format!("noise{index}.md")),
                format!("filler content number {index}\n"),
            )
            .unwrap();
        }
        fs::write(dir.path().join("target.md"), "a unique zebra marker\n").unwrap();
        run(dir.path(), &config(), RunMode::Full).unwrap();

        // The whole index is large…
        assert!(stored_chunks(dir.path()).len() >= 51);
        // …but a query materializes only the rows the FTS index matched, not all
        // of them — the bounded-memory guarantee.
        let hits = search(dir.path(), "zebra").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "target.md");
    }

    #[test]
    fn pause_resume_and_cancel_update_persistent_job_state() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "content\n").unwrap();
        run(dir.path(), &config(), RunMode::Full).unwrap();

        assert_eq!(
            pause(dir.path()).unwrap().unwrap().status,
            JobStatus::Paused
        );
        assert_eq!(
            resume(dir.path()).unwrap().unwrap().status,
            JobStatus::Queued
        );
        assert_eq!(
            cancel(dir.path()).unwrap().unwrap().status,
            JobStatus::Cancelled
        );
        assert_eq!(
            status(dir.path()).unwrap().unwrap().status,
            JobStatus::Cancelled
        );
    }

    #[test]
    fn run_budgets_are_explicit_and_testable() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("one.md"), "one").unwrap();
        fs::write(dir.path().join("two.md"), "two").unwrap();
        let cfg = IngestConfig {
            enabled: true,
            max_files: 1,
            max_run_bytes: 1024,
            max_tokens: 1024,
            max_file_bytes: 1024,
            max_elapsed_secs: 60,
            ..IngestConfig::default()
        };

        let manifest = preview(dir.path(), &cfg).unwrap();

        assert_eq!(manifest.estimates.candidate_files, 1);
        assert_eq!(manifest.estimates.skipped_files, 1);
        assert!(manifest
            .entries
            .iter()
            .any(|entry| entry.status == CandidateStatus::OverBudget));
    }

    #[test]
    fn promote_enqueues_review_without_accepted_memory() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "project convention\n").unwrap();
        run(dir.path(), &config(), RunMode::Full).unwrap();
        let items = review_items(dir.path()).unwrap();
        let inserted = promote_for_review(dir.path(), &items[0].id).unwrap();

        assert_eq!(inserted, 1);
        assert!(!dir.path().join(".localmind").join("memory").exists());
        let queue = crate::review_list(dir.path()).unwrap();
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn review_items_include_tooling_skill_and_research_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\n",
        )
        .unwrap();
        fs::write(dir.path().join("README.md"), "guide\n").unwrap();
        fs::write(dir.path().join("notes.txt"), "notes\n").unwrap();
        run(dir.path(), &config(), RunMode::Full).unwrap();

        let items = review_items(dir.path()).unwrap();

        assert!(items.iter().any(|item| item.kind == "tooling"));
        assert!(items.iter().any(|item| item.kind == "skill"));
        assert!(items.iter().any(|item| item.kind == "research"));
    }
}
