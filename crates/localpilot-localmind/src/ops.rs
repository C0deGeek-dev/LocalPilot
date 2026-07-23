//! Review-queue, memory, and audit operations over the LocalMind store.
//!
//! These wrap LocalMind's project store and return plain LocalPilot-owned types
//! so callers (the CLI) never name a LocalMind type directly.

use localmind_core::{MemoryEntryId, ReviewAction, ReviewDecision, ReviewItemId, SkillDraftId};
use localmind_store::{
    is_revalidation_candidate, FreshnessReport, FreshnessScope, FreshnessThresholds,
    MemoryPersistence, MemoryPersistenceError, MemoryRecord, RevalidationConfig, ReviewQueue,
    ReviewQueueItem, SkillDraftRecord, SkillDraftStore, StoreConfigError,
};

use crate::LearningError;
use std::path::Path;

/// A review-queue item, flattened for display.
#[derive(Debug, Clone)]
pub struct ReviewSummary {
    pub id: String,
    pub state: String,
    pub session_id: String,
    pub summary: String,
    pub category: String,
    pub confidence: f32,
    pub note: Option<String>,
    pub replacement: Option<String>,
    /// How many times this candidate (or a near-duplicate) was proposed; dedup at
    /// enqueue bumps this instead of stacking rows.
    pub seen_count: i64,
    /// Full carried source evidence (shown by review surfaces under the
    /// summary; never written into a promoted memory body).
    pub evidence_text: Option<String>,
    /// The summary is a source excerpt a reviewer must edit into a standalone
    /// lesson before promotion.
    pub requires_edit: bool,
}

/// A reviewer's verdict on a queue item.
#[derive(Debug, Clone)]
pub enum ReviewVerdict {
    Accept,
    Reject,
    Defer,
    Edit { replacement: String },
}

/// A memory search hit.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    pub memory_id: String,
    pub score: i64,
    pub path: String,
    pub snippet: String,
    /// The memory's lesson category, so a caller can gate or dedup injection by
    /// category without a second store lookup.
    pub category: String,
    /// Normalized cosine similarity of the prompt to this lesson's stored
    /// embedding vector, when an embedding endpoint is configured and the lesson
    /// has a vector. `None` when embeddings are unavailable or the lesson is
    /// unembedded — the injection relevance gate then lets the hit pass (the
    /// best-effort keyword path), so a no-embed run is byte-identical to today.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cosine: Option<f32>,
}

/// An accepted LocalMind memory entry, flattened for display.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemorySummary {
    pub id: String,
    pub scope: String,
    pub category: String,
    pub status: String,
    pub path: String,
    pub body: String,
    /// Times injected into a turn (0 = never retrieved).
    pub hit_count: i64,
    /// When last injected, or `None` if never.
    pub last_used_at: Option<String>,
    /// Flagged for review (change-aware staleness or the freshness pass).
    pub stale_candidate: bool,
    /// In a `contradicts` relationship with another memory.
    pub contradicted: bool,
}

fn memory_summary(record: MemoryRecord) -> MemorySummary {
    MemorySummary {
        id: record.memory_id.to_string(),
        scope: record.scope,
        category: record.category,
        status: record.status,
        path: record.path.display().to_string(),
        body: record.body,
        hit_count: record.hit_count,
        last_used_at: record.last_used_at,
        stale_candidate: record.stale_candidate,
        contradicted: record.contradicted,
    }
}

/// An audit-log entry for a memory change.
#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub id: i64,
    pub kind: String,
    pub actor: String,
    pub subject: String,
    pub at: String,
}

fn summarize(item: &ReviewQueueItem) -> ReviewSummary {
    ReviewSummary {
        id: item.id.to_string(),
        state: format!("{:?}", item.state),
        session_id: item.session_id.to_string(),
        summary: item.candidate.summary().to_string(),
        category: format!("{:?}", item.candidate.category),
        confidence: item.candidate.confidence.value(),
        note: item.note.clone(),
        replacement: item.replacement_summary.clone(),
        seen_count: item.seen_count,
        evidence_text: item.candidate.evidence_text.clone(),
        requires_edit: item.candidate.requires_edit_before_promotion,
    }
}

/// List every item in the project's review queue.
///
/// # Errors
/// Returns [`LearningError::Review`] if the queue cannot be opened or read.
pub fn review_list(project_root: &Path) -> Result<Vec<ReviewSummary>, LearningError> {
    let queue = open_queue(project_root)?;
    let items = queue.list().map_err(review_err)?;
    Ok(items.iter().map(summarize).collect())
}

/// List the project's review queue **without** creating any project files. A
/// project that has never closed out (no `.localmind.toml`) has an empty queue,
/// so this returns an empty list instead of initializing the store — keeping it
/// safe to call from a read-only, model-facing surface on a bare prompt.
///
/// # Errors
/// Returns [`LearningError::Review`] if an existing queue cannot be read.
pub fn review_list_readonly(project_root: &Path) -> Result<Vec<ReviewSummary>, LearningError> {
    if !project_root.join(crate::CONFIG_FILE).exists() {
        return Ok(Vec::new());
    }
    let queue = ReviewQueue::open_project(project_root).map_err(review_err)?;
    let items = queue.list().map_err(review_err)?;
    Ok(items.iter().map(summarize).collect())
}

/// Inspect a single review-queue item.
///
/// # Errors
/// Returns [`LearningError::Review`] if the queue cannot be opened or read.
pub fn review_show(
    project_root: &Path,
    item_id: &str,
) -> Result<Option<ReviewSummary>, LearningError> {
    let queue = open_queue(project_root)?;
    let item = queue.get(&ReviewItemId::new(item_id)).map_err(review_err)?;
    Ok(item.as_ref().map(summarize))
}

/// Delete every pending review candidate, returning how many rows were removed.
/// Accepted/rejected/edited items and all accepted-memory tables are untouched —
/// this clears only the un-reviewed backlog. Back up the store first (the CLI
/// does) since this is irreversible.
///
/// # Errors
/// Returns [`LearningError::Review`] if the queue cannot be opened or purged.
pub fn review_purge(project_root: &Path) -> Result<usize, LearningError> {
    let queue = open_queue(project_root)?;
    queue.purge_pending().map_err(review_err)
}

/// Cluster pending review candidates by lexical similarity so near-duplicates
/// can be triaged together. Each returned group holds the indices (into a
/// caller-held list) of summaries that are mutual near-duplicates; singletons
/// form their own group. Deterministic and offline.
#[must_use]
pub fn cluster_by_similarity(summaries: &[String]) -> Vec<Vec<usize>> {
    let token_sets: Vec<_> = summaries
        .iter()
        .map(|summary| localmind_store::token_set(summary))
        .collect();
    let mut assigned = vec![false; summaries.len()];
    let mut clusters = Vec::new();
    for seed in 0..summaries.len() {
        if assigned[seed] {
            continue;
        }
        assigned[seed] = true;
        let mut cluster = vec![seed];
        for other in (seed + 1)..summaries.len() {
            if !assigned[other]
                && localmind_store::similarity(&token_sets[seed], &token_sets[other])
                    >= localmind_store::NEAR_DUP_THRESHOLD
            {
                assigned[other] = true;
                cluster.push(other);
            }
        }
        clusters.push(cluster);
    }
    clusters
}

/// Record a reviewer's verdict on a queue item, returning the new state.
///
/// # Errors
/// Returns [`LearningError`] if the decision or its audit record fails.
pub fn review_decide(
    project_root: &Path,
    item_id: &str,
    verdict: ReviewVerdict,
    reviewer: &str,
    note: Option<String>,
) -> Result<String, LearningError> {
    let (action, replacement_summary) = match verdict {
        ReviewVerdict::Accept => (ReviewAction::Accept, None),
        ReviewVerdict::Reject => (ReviewAction::Reject, None),
        ReviewVerdict::Defer => (ReviewAction::MarkTemporary, None),
        ReviewVerdict::Edit { replacement } => (ReviewAction::Edit, Some(replacement)),
    };
    let persistence = open_memory(project_root)?;
    let queue = open_queue(project_root)?;
    let item = queue
        .decide(ReviewDecision {
            item_id: ReviewItemId::new(item_id),
            action,
            reviewer: reviewer.to_string(),
            decided_at: None,
            note,
            replacement_summary,
            evidence: Vec::new(),
        })
        .map_err(review_err)?;
    persistence
        .record_review_item_audit(&item)
        .map_err(memory_err)?;
    Ok(format!("{:?}", item.state))
}

/// Promote an accepted review item into durable Markdown memory, returning the
/// new memory entry id.
///
/// # Errors
/// Returns [`LearningError::Memory`] if promotion fails.
pub fn promote(project_root: &Path, item_id: &str) -> Result<String, LearningError> {
    let persistence = open_memory(project_root)?;
    let entry = persistence
        .promote_review_item(&ReviewItemId::new(item_id))
        .map_err(memory_err)?;

    // Anchor the accepted memory to the code nodes its hints resolve to, so
    // graph retrieval can pull it in by structure. Best-effort: a memory that
    // anchors nowhere is still promoted.
    let mut hints = entry.related_entities.clone();
    hints.extend(entry.related_files.clone());
    if !hints.is_empty() {
        if let Ok(store) = localmind_store::GraphStore::open_project(project_root) {
            let _ = localmind_codegraph::anchor_memory(&store, &entry.id, &hints);
        }
    }
    Ok(entry.id.to_string())
}

/// Search accepted memory.
///
/// # Errors
/// Returns [`LearningError::Memory`] if the search fails.
pub fn search(project_root: &Path, query: &str) -> Result<Vec<SearchHit>, LearningError> {
    let persistence = open_memory(project_root)?;
    let results = persistence.search(query).map_err(memory_err)?;
    Ok(results
        .into_iter()
        .map(|result| SearchHit {
            memory_id: result.memory_id.to_string(),
            score: result.score,
            path: result.path.display().to_string(),
            snippet: result.snippet,
            category: result.category,
            cosine: None,
        })
        .collect())
}

/// Search accepted memory **without** creating any project files. A project that
/// has never closed out (no `.localmind.toml`) has no accepted memory, so this
/// returns an empty list instead of initializing the store — safe to call from a
/// read-only, model-facing surface on a bare prompt.
///
/// # Errors
/// Returns [`LearningError::Memory`] if an existing memory index cannot be read.
pub fn search_readonly(project_root: &Path, query: &str) -> Result<Vec<SearchHit>, LearningError> {
    if !project_root.join(crate::CONFIG_FILE).exists() {
        return Ok(Vec::new());
    }
    let persistence = MemoryPersistence::open_project(project_root).map_err(memory_err)?;
    let results = persistence.search(query).map_err(memory_err)?;
    Ok(results
        .into_iter()
        .map(|result| SearchHit {
            memory_id: result.memory_id.to_string(),
            score: result.score,
            path: result.path.display().to_string(),
            snippet: result.snippet,
            category: result.category,
            cosine: None,
        })
        .collect())
}

/// The accepted-memory hits that always-on context injection draws on for a
/// prompt — the single ranked, capped retrieval that backs *both* the injected
/// block and the "memories used" audit, so the two can never diverge. Capped at
/// [`CONTEXT_MEMORY_LIMIT`] (the count actually injected). Empty when injection
/// is disabled, the project has no LocalMind config, or learning is disabled;
/// never creates project files. A present-but-broken store is *not* empty — see
/// Errors.
///
/// # Errors
/// Returns [`LearningError::Context`] when an existing store cannot be read —
/// malformed config, a failed migration, or database corruption — so a broken
/// store surfaces as an actionable error instead of masquerading as "no memory".
/// A missing config or disabled learning is an empty result, not an error.
pub fn context_hits(
    project_root: &Path,
    query: &str,
    language: Option<&str>,
) -> Result<Vec<SearchHit>, LearningError> {
    if !memory_injection_enabled(project_root) {
        return Ok(Vec::new());
    }
    // Distinguish "nothing to inject" from "the store is broken". A project that
    // has never closed out (no config) or has learning disabled has no memory —
    // an empty result. Any other open failure (malformed config, failed
    // migration, corrupt database) is propagated so corruption cannot silently
    // remove memory from context and from the "memories used" evidence alike.
    let persistence = match MemoryPersistence::open_project(project_root) {
        Ok(persistence) => persistence,
        Err(MemoryPersistenceError::Config(
            StoreConfigError::MissingConfig { .. } | StoreConfigError::LearningDisabled { .. },
        )) => return Ok(Vec::new()),
        Err(e) => return Err(LearningError::Context(e.to_string())),
    };
    let hits = persistence
        .search_lang(query, language)
        .map_err(|e| LearningError::Context(e.to_string()))?;
    // Best-effort semantic relevance: embed the prompt once and score the
    // candidate memories by cosine over the stored vectors, so the injection gate
    // can drop a same-language but off-topic lesson. The keyword bm25 search above
    // stays the candidate floor — cosine only re-filters it, never selects. When
    // no embedding endpoint is configured (or it is unreachable, or a candidate
    // has no stored vector) the cosine is `None` and the hit is injected exactly
    // as today, so a no-embed run is byte-identical.
    let cosines = relevance_cosines(&persistence, query);
    // The opt-in rerank stage (`[retrieval] rerank` + an embedding endpoint):
    // reorder the top keyword candidates by the same stored-vector cosines the
    // gate uses, through the engine's rerank policy — keyword stays the
    // candidate floor, a hit without a stored vector keeps its slot, and with
    // the flag off (the default) the order is byte-identical.
    let hits = match (&cosines, active_rerank_window(project_root)) {
        (Some(by_id), Some(window)) => localmind_search::rerank_scored(hits, window, |hit| {
            by_id.get(&hit.memory_id.to_string()).copied()
        }),
        _ => hits,
    };
    Ok(hits
        .into_iter()
        .take(CONTEXT_MEMORY_LIMIT)
        .map(|hit| {
            let memory_id = hit.memory_id.to_string();
            let cosine = cosines
                .as_ref()
                .and_then(|by_id| by_id.get(&memory_id).copied());
            SearchHit {
                memory_id,
                score: hit.score,
                path: hit.path.display().to_string(),
                snippet: hit.snippet,
                category: hit.category,
                cosine,
            }
        })
        .collect())
}

/// The rerank window, when the project opts in (`[retrieval] rerank = true`
/// **and** an embedding endpoint is configured — `ProjectConfig::rerank_active`
/// is the single gate). `None` leaves the keyword blend order untouched, the
/// default posture.
fn active_rerank_window(project_root: &Path) -> Option<usize> {
    let config = localmind_store::ProjectConfig::discover(project_root).ok()?;
    config.rerank_active().then(|| config.rerank_window())
}

/// Cosine similarity of `query` to each accepted memory that has a stored vector,
/// keyed by `memory_id`. Best-effort and offline-safe: `None` when no embedding
/// endpoint is configured, it is unreachable, or no vectors are stored — the
/// caller then attaches no cosine and the keyword path is unchanged. Reuses the
/// engine's `embed_query` + global-aware `vector_search` (the same primitives the
/// review-mode semantic dedup uses); no new retrieval engine.
fn relevance_cosines(
    persistence: &MemoryPersistence,
    query: &str,
) -> Option<std::collections::HashMap<String, f32>> {
    let vector = persistence.embed_query(query).ok().flatten()?;
    let scored = persistence
        .vector_search(&vector, RELEVANCE_VECTOR_WINDOW)
        .ok()?;
    Some(
        scored
            .into_iter()
            .filter(|result| result.subject_kind == "memory")
            .map(|result| (result.subject_id, result.score))
            .collect(),
    )
}

/// How many nearest vectors to score for the injection relevance gate. Generous
/// headroom over the `CONTEXT_MEMORY_LIMIT` keyword candidates because the
/// `vector_index` also holds non-memory subjects (ingested code chunks); a memory
/// candidate ranked below this window simply carries no cosine and passes the gate
/// (the conservative direction — under-gate rather than over-exclude). Mirrors the
/// dedup path's `max(20)` candidate window.
const RELEVANCE_VECTOR_WINDOW: usize = 64;

/// The number of accepted-memory hits injected into a turn's context — the cap
/// the audit record must share so it never lists a memory that was not injected.
pub const CONTEXT_MEMORY_LIMIT: usize = 5;

/// Record that `memories` were injected into a turn, bumping each one's usage
/// count. **Best-effort and post-turn**: driven from the turn-exit, never the
/// retrieval read path, and every failure is swallowed so a usage write can
/// never fail a turn. Synthetic ids (the repository primer, ingest chunks) and
/// unknown ids simply match no memory row. A no-op when the set is empty or no
/// store can be opened.
pub fn record_memory_usage(project_root: &Path, memories: &[localpilot_store::MemoryUsed]) {
    if memories.is_empty() {
        return;
    }
    let Ok(persistence) = MemoryPersistence::open_project(project_root) else {
        return;
    };
    let ids: Vec<MemoryEntryId> = memories
        .iter()
        .map(|memory| MemoryEntryId::new(memory.id.clone()))
        .collect();
    if let Err(error) = persistence.record_memory_usage(&ids) {
        tracing::warn!(
            target: "localpilot::localmind",
            %error,
            "best-effort memory-usage bump failed; the turn is unaffected"
        );
    }
}

/// Down-weight a lesson by routing it to review — never deleting it. The host's
/// learning loop calls this when the uplift eval shows a lesson did not improve
/// (or hurt) outcomes; it reuses the engine's reasoned route-to-review flag, so
/// the memory stays active but is surfaced for a human to re-judge. Returns
/// whether an active memory matched.
///
/// # Errors
/// Returns [`LearningError::Memory`] if the store cannot be read or updated.
pub fn flag_unhelpful_lesson(project_root: &Path, memory_id: &str) -> Result<bool, LearningError> {
    let persistence = open_memory(project_root)?;
    persistence
        .flag_for_review(
            &MemoryEntryId::new(memory_id),
            "did not improve eval outcomes",
        )
        .map_err(memory_err)
}

/// The lessons currently flagged for review (down-weighted or change-invalidated),
/// the review list a host surfaces. Never includes a deleted memory.
///
/// # Errors
/// Returns [`LearningError::Memory`] if the store cannot be read.
pub fn lessons_flagged_for_review(project_root: &Path) -> Result<Vec<String>, LearningError> {
    let persistence = open_memory(project_root)?;
    Ok(persistence
        .list_stale_candidates()
        .map_err(memory_err)?
        .into_iter()
        .map(|id| id.to_string())
        .collect())
}

/// List accepted LocalMind memory.
///
/// # Errors
/// Returns [`LearningError::Memory`] if the memory index cannot be read.
pub fn memory_list(project_root: &Path) -> Result<Vec<MemorySummary>, LearningError> {
    let persistence = open_memory(project_root)?;
    let records = persistence.list_memory().map_err(memory_err)?;
    Ok(records.into_iter().map(memory_summary).collect())
}

/// Host-owned freshness thresholds for the operator CLI (a flat mirror of the
/// engine's, so the CLI never names a LocalMind type). `None` fields fall back to
/// the engine defaults.
#[derive(Debug, Clone, Default)]
pub struct FreshnessParams {
    pub max_age_days: Option<i64>,
    pub unused_grace_days: Option<i64>,
    pub version_sensitive_min_age_days: Option<i64>,
    pub max_flags: Option<usize>,
}

/// One memory the freshness pass selected for review, flattened for display.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FreshnessFlagOut {
    pub memory_id: String,
    pub reason: String,
}

/// The outcome of a freshness pass, flattened + serializable for `--format json`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FreshnessOutcome {
    pub scanned: usize,
    pub version_sensitive: usize,
    pub unused: usize,
    pub age: usize,
    pub total_candidates: usize,
    pub capped: bool,
    pub dry_run: bool,
    pub flagged: Vec<FreshnessFlagOut>,
}

impl From<FreshnessReport> for FreshnessOutcome {
    fn from(report: FreshnessReport) -> Self {
        Self {
            scanned: report.scanned,
            version_sensitive: report.version_sensitive,
            unused: report.unused,
            age: report.age,
            total_candidates: report.total_candidates(),
            capped: report.capped,
            dry_run: report.dry_run,
            flagged: report
                .flagged
                .into_iter()
                .map(|flag| FreshnessFlagOut {
                    memory_id: flag.memory_id,
                    reason: flag.reason.as_str().to_string(),
                })
                .collect(),
        }
    }
}

/// Run the deterministic freshness pass: flag accepted memory for review by age,
/// never-retrieved-after-grace, and version-sensitivity. `scope` is
/// `project`/`global`/`both`; with `dry_run` it reports candidates without
/// writing. Routes to the existing review gate — never deletes.
///
/// # Errors
/// Returns [`LearningError::Memory`] if the store cannot be read/updated, or the
/// scope token is invalid.
pub fn freshness_pass(
    project_root: &Path,
    params: &FreshnessParams,
    scope: &str,
    dry_run: bool,
) -> Result<FreshnessOutcome, LearningError> {
    let scope = FreshnessScope::parse(scope).ok_or_else(|| {
        LearningError::Memory(format!("invalid scope {scope:?}; use project|global|both"))
    })?;
    let defaults = FreshnessThresholds::default();
    let thresholds = FreshnessThresholds {
        max_age_days: params.max_age_days.unwrap_or(defaults.max_age_days),
        unused_grace_days: params
            .unused_grace_days
            .unwrap_or(defaults.unused_grace_days),
        version_sensitive_min_age_days: params
            .version_sensitive_min_age_days
            .unwrap_or(defaults.version_sensitive_min_age_days),
        max_flags: params.max_flags.unwrap_or(defaults.max_flags),
    };
    let persistence = open_memory(project_root)?;
    let report = persistence
        .freshness_pass(&thresholds, scope, dry_run)
        .map_err(memory_err)?;
    Ok(FreshnessOutcome::from(report))
}

/// The memory lifecycle review queues, derived from a single store read: stale
/// candidates (flagged for review), never-retrieved (dead-weight), most-used
/// (high-value), and contradicted. The act path for any of these stays the
/// existing review/delete CLI — this only *surfaces* them (operator-invoked,
/// never auto-deleting).
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryLifecycle {
    pub total: usize,
    pub stale: Vec<MemorySummary>,
    pub never_retrieved: Vec<MemorySummary>,
    pub most_used: Vec<MemorySummary>,
    pub contradicted: Vec<MemorySummary>,
}

/// Assemble the memory lifecycle listing. `most_used_limit` caps the most-used
/// section.
///
/// # Errors
/// Returns [`LearningError::Memory`] if the memory index cannot be read.
pub fn memory_lifecycle(
    project_root: &Path,
    most_used_limit: usize,
) -> Result<MemoryLifecycle, LearningError> {
    let persistence = open_memory(project_root)?;
    let records = persistence.list_memory().map_err(memory_err)?;
    let total = records.len();
    let summaries: Vec<MemorySummary> = records.into_iter().map(memory_summary).collect();

    let stale = summaries
        .iter()
        .filter(|m| m.stale_candidate)
        .cloned()
        .collect();
    let never_retrieved = summaries
        .iter()
        .filter(|m| m.hit_count == 0)
        .cloned()
        .collect();
    let contradicted = summaries
        .iter()
        .filter(|m| m.contradicted)
        .cloned()
        .collect();
    let mut most_used: Vec<MemorySummary> = summaries
        .iter()
        .filter(|m| m.hit_count > 0)
        .cloned()
        .collect();
    most_used.sort_by(|a, b| b.hit_count.cmp(&a.hit_count).then_with(|| a.id.cmp(&b.id)));
    most_used.truncate(most_used_limit);

    Ok(MemoryLifecycle {
        total,
        stale,
        never_retrieved,
        most_used,
        contradicted,
    })
}

/// The outcome of a source re-validation invocation, flattened + serializable.
/// On a preview (no `--apply`) only `candidates` is set and no model is contacted;
/// on apply the remaining fields report the model's verdicts.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RevalidationOutcome {
    /// Version-sensitive accepted lessons eligible for re-validation (computed
    /// offline, always).
    pub candidates: usize,
    /// Whether the configured model was actually contacted (only on apply).
    pub contacted_model: bool,
    /// Whether a chat model is configured (false → the live pass is unavailable).
    pub model_available: bool,
    pub sampled: usize,
    pub no_longer_true: usize,
    pub still_current: usize,
    pub unknown: usize,
    /// Ids routed to review (the "no longer true" verdicts).
    pub flagged: Vec<String>,
}

/// Optional, opt-in source re-validation. **Default-off and network-touching**
/// (egress is the caller's disclosed choice): a preview (`apply = false`) counts
/// version-sensitive candidates **offline** and contacts nothing; only
/// `apply = true` contacts the configured model and routes "no longer true"
/// verdicts to review (never deletes). The live model run is opportunistic — the
/// logic is offline-tested with a
/// fixture verdict source in the engine.
///
/// # Errors
/// Returns [`LearningError::Memory`] if the store cannot be read or updated.
pub fn revalidate(
    project_root: &Path,
    sample_size: usize,
    apply: bool,
) -> Result<RevalidationOutcome, LearningError> {
    let persistence = open_memory(project_root)?;
    // Offline candidate count — never contacts the network.
    let candidates = persistence
        .list_memory()
        .map_err(memory_err)?
        .into_iter()
        .filter(|record| is_revalidation_candidate(&record.body))
        .count();

    if !apply {
        return Ok(RevalidationOutcome {
            candidates,
            contacted_model: false,
            model_available: false,
            sampled: 0,
            no_longer_true: 0,
            still_current: 0,
            unknown: 0,
            flagged: Vec::new(),
        });
    }

    let config = RevalidationConfig { sample_size };
    match persistence
        .revalidate_with_model(&config, false)
        .map_err(memory_err)?
    {
        None => Ok(RevalidationOutcome {
            candidates,
            contacted_model: false,
            model_available: false,
            sampled: 0,
            no_longer_true: 0,
            still_current: 0,
            unknown: 0,
            flagged: Vec::new(),
        }),
        Some(report) => Ok(RevalidationOutcome {
            candidates,
            contacted_model: true,
            model_available: true,
            sampled: report.sampled,
            no_longer_true: report.no_longer_true,
            still_current: report.still_current,
            unknown: report.unknown,
            flagged: report.flagged,
        }),
    }
}

/// Delete accepted LocalMind memory by id.
///
/// # Errors
/// Returns [`LearningError::Memory`] if deletion fails.
pub fn memory_delete(project_root: &Path, id: &str) -> Result<bool, LearningError> {
    let persistence = open_memory(project_root)?;
    persistence
        .delete_memory(&MemoryEntryId::new(id), "localpilot")
        .map_err(memory_err)
}

/// Whether LocalMind context injection is enabled for this project.
#[must_use]
pub fn memory_injection_enabled(project_root: &Path) -> bool {
    !injection_disabled_path(project_root).exists()
}

/// Disable LocalMind context injection for this project without disabling the
/// review/promotion store itself.
///
/// # Errors
/// Returns [`LearningError::Memory`] if the flag cannot be written.
pub fn memory_disable_injection(project_root: &Path) -> Result<(), LearningError> {
    let state_dir = project_root.join(".localmind");
    std::fs::create_dir_all(&state_dir).map_err(memory_err)?;
    std::fs::write(
        injection_disabled_path(project_root),
        b"context injection disabled\n",
    )
    .map_err(memory_err)
}

/// Re-enable LocalMind context injection for this project by clearing the
/// disable flag. Idempotent: a no-op when injection is already enabled.
///
/// # Errors
/// Returns [`LearningError::Memory`] if the flag file exists but cannot be removed.
pub fn memory_enable_injection(project_root: &Path) -> Result<(), LearningError> {
    let path = injection_disabled_path(project_root);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(memory_err(e)),
    }
}

/// Retrieve relevant accepted memory for `query`, formatted as a compact context
/// block to seed into an agent turn. Returns `None` when nothing matches, so the
/// caller injects nothing rather than noise.
///
/// # Errors
/// Returns [`LearningError::Context`] if memory cannot be searched.
pub fn context_for(project_root: &Path, query: &str) -> Result<Option<String>, LearningError> {
    use std::fmt::Write as _;
    let hits = context_hits(project_root, query, None)?;
    if hits.is_empty() {
        return Ok(None);
    }
    let mut context = String::from("Relevant accepted project memory:\n");
    for hit in &hits {
        let _ = writeln!(context, "- {}", hit.snippet.trim());
    }
    Ok(Some(context))
}

/// The memory-change audit log.
///
/// # Errors
/// Returns [`LearningError::Memory`] if the audit log cannot be read.
pub fn audit(project_root: &Path) -> Result<Vec<AuditEntry>, LearningError> {
    let persistence = open_memory(project_root)?;
    let records = persistence.audit_records().map_err(memory_err)?;
    Ok(records
        .into_iter()
        .map(|record| AuditEntry {
            id: record.id,
            kind: record.kind,
            actor: record.actor,
            subject: record.subject,
            at: record.happened_at,
        })
        .collect())
}

/// A generated skill draft, flattened for display.
#[derive(Debug, Clone)]
pub struct SkillDraftInfo {
    pub id: String,
    pub name: String,
    pub disabled: bool,
    pub description: String,
    pub path: String,
}

/// A host-consumable active skill.
#[derive(Debug, Clone)]
pub struct ActiveSkillInfo {
    pub id: String,
    pub name: String,
    pub body_markdown: String,
}

fn draft_info(record: &SkillDraftRecord) -> SkillDraftInfo {
    SkillDraftInfo {
        id: record.draft.id.to_string(),
        name: record.draft.name.clone(),
        disabled: record.draft.disabled,
        description: record.draft.description.clone(),
        path: record.draft_path.display().to_string(),
    }
}

/// Generate disabled skill drafts from accepted review items.
///
/// # Errors
/// Returns [`LearningError::Skill`] if generation fails.
pub fn skills_generate(project_root: &Path) -> Result<Vec<SkillDraftInfo>, LearningError> {
    let store = open_skills(project_root)?;
    let records = store.generate_from_review_queue().map_err(skill_err)?;
    Ok(records.iter().map(draft_info).collect())
}

/// List generated skill drafts.
///
/// # Errors
/// Returns [`LearningError::Skill`] if the drafts cannot be read.
pub fn skills_list(project_root: &Path) -> Result<Vec<SkillDraftInfo>, LearningError> {
    let store = open_skills(project_root)?;
    let records = store.list().map_err(skill_err)?;
    Ok(records.iter().map(draft_info).collect())
}

/// List active LocalMind skills that LocalPilot may inject as host context.
///
/// # Errors
/// Returns [`LearningError::Skill`] if the active skill store cannot be read.
pub fn skills_active(project_root: &Path) -> Result<Vec<ActiveSkillInfo>, LearningError> {
    let store = open_skills(project_root)?;
    let records = store.active().map_err(skill_err)?;
    Ok(records
        .into_iter()
        .map(|record| ActiveSkillInfo {
            id: record.skill.id.to_string(),
            name: record.skill.name,
            body_markdown: record.skill.body_markdown,
        })
        .collect())
}

/// Enable (activate) a skill draft — the deliberate human step that turns a
/// disabled draft into an active, host-consumable skill. Returns the resulting
/// active skill, or `None` when no draft has that id.
///
/// # Errors
/// Returns [`LearningError::Skill`] if the store cannot be read or written.
pub fn skill_activate(
    project_root: &Path,
    draft_id: &str,
) -> Result<Option<ActiveSkillInfo>, LearningError> {
    let store = open_skills(project_root)?;
    let record = store
        .activate(&SkillDraftId::new(draft_id))
        .map_err(skill_err)?;
    Ok(record.map(|record| ActiveSkillInfo {
        id: record.skill.id.to_string(),
        name: record.skill.name,
        body_markdown: record.skill.body_markdown,
    }))
}

/// Inspect a single skill draft.
///
/// # Errors
/// Returns [`LearningError::Skill`] if the draft cannot be read.
pub fn skill_show(
    project_root: &Path,
    draft_id: &str,
) -> Result<Option<SkillDraftInfo>, LearningError> {
    let store = open_skills(project_root)?;
    let record = store.get(&SkillDraftId::new(draft_id)).map_err(skill_err)?;
    Ok(record.as_ref().map(draft_info))
}

/// The Markdown body of a skill draft, for export.
///
/// # Errors
/// Returns [`LearningError::Skill`] if the draft cannot be read.
pub fn skill_body(project_root: &Path, draft_id: &str) -> Result<Option<String>, LearningError> {
    let store = open_skills(project_root)?;
    let record = store.get(&SkillDraftId::new(draft_id)).map_err(skill_err)?;
    Ok(record.map(|record| record.draft.body_markdown))
}

/// List generated skill drafts **without** creating any project files. A project
/// that has never closed out (no `.localmind.toml`) has no drafts, so this
/// returns an empty list instead of initializing the store — keeping it safe to
/// call from a read-only, model-facing surface on a bare prompt.
///
/// # Errors
/// Returns [`LearningError::Skill`] if an existing draft store cannot be read.
pub fn skill_drafts_readonly(project_root: &Path) -> Result<Vec<SkillDraftInfo>, LearningError> {
    let Some(store) = open_skills_readonly(project_root)? else {
        return Ok(Vec::new());
    };
    let records = store.list().map_err(skill_err)?;
    Ok(records.iter().map(draft_info).collect())
}

/// Inspect a single skill draft — its display info and Markdown body — without
/// creating project files. Read-only; returns `None` when the project has no
/// store yet or no draft with that id.
///
/// # Errors
/// Returns [`LearningError::Skill`] if an existing draft store cannot be read.
pub fn skill_draft_detail_readonly(
    project_root: &Path,
    draft_id: &str,
) -> Result<Option<(SkillDraftInfo, String)>, LearningError> {
    let Some(store) = open_skills_readonly(project_root)? else {
        return Ok(None);
    };
    let record = store.get(&SkillDraftId::new(draft_id)).map_err(skill_err)?;
    Ok(record.map(|record| (draft_info(&record), record.draft.body_markdown)))
}

/// List **active** (enabled) skills without creating project files. The
/// read-only, model-facing counterpart of [`skills_active`]: a project with no
/// store yet returns an empty list instead of initializing one.
///
/// # Errors
/// Returns [`LearningError::Skill`] if an existing store cannot be read.
pub fn skills_active_readonly(project_root: &Path) -> Result<Vec<ActiveSkillInfo>, LearningError> {
    let Some(store) = open_skills_readonly(project_root)? else {
        return Ok(Vec::new());
    };
    let records = store.active().map_err(skill_err)?;
    Ok(records
        .into_iter()
        .map(|record| ActiveSkillInfo {
            id: record.skill.id.to_string(),
            name: record.skill.name,
            body_markdown: record.skill.body_markdown,
        })
        .collect())
}

/// Open the review queue, ensuring the project has a LocalMind config first so a
/// never-closed-out project opens an empty queue rather than erroring.
fn open_queue(project_root: &Path) -> Result<ReviewQueue, LearningError> {
    crate::initialize(project_root)?;
    ReviewQueue::open_project(project_root).map_err(review_err)
}

/// Open memory persistence, ensuring the project is initialized first.
pub(crate) fn open_memory(project_root: &Path) -> Result<MemoryPersistence, LearningError> {
    crate::initialize(project_root)?;
    MemoryPersistence::open_project(project_root).map_err(memory_err)
}

/// Open the skill-draft store, ensuring the project is initialized first.
fn open_skills(project_root: &Path) -> Result<SkillDraftStore, LearningError> {
    crate::initialize(project_root)?;
    SkillDraftStore::open_project(project_root).map_err(skill_err)
}

/// Open the skill-draft store **only if** the project already has a LocalMind
/// config, so a read-only caller never creates `.localmind.toml`. Returns `None`
/// when the project has no store yet.
fn open_skills_readonly(project_root: &Path) -> Result<Option<SkillDraftStore>, LearningError> {
    if !project_root.join(crate::CONFIG_FILE).exists() {
        return Ok(None);
    }
    SkillDraftStore::open_project(project_root)
        .map(Some)
        .map_err(skill_err)
}

fn review_err(e: impl std::fmt::Display) -> LearningError {
    LearningError::Review(e.to_string())
}

fn memory_err(e: impl std::fmt::Display) -> LearningError {
    LearningError::Memory(e.to_string())
}

fn skill_err(e: impl std::fmt::Display) -> LearningError {
    LearningError::Skill(e.to_string())
}

fn injection_disabled_path(project_root: &Path) -> std::path::PathBuf {
    project_root
        .join(".localmind")
        .join("context-injection-disabled")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injection_toggle_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(memory_injection_enabled(root), "enabled by default");
        memory_disable_injection(root).unwrap();
        assert!(!memory_injection_enabled(root), "disabled after disable");
        memory_enable_injection(root).unwrap();
        assert!(memory_injection_enabled(root), "enabled after enable");
        // Enable is idempotent — a second call on an already-enabled project is a no-op.
        memory_enable_injection(root).unwrap();
        assert!(memory_injection_enabled(root));
    }

    #[test]
    fn clusters_group_near_duplicates_and_isolate_distinct_lessons() {
        let summaries = vec![
            "run the integration suite after every exporter change".to_string(),
            "after an exporter change, run the integration suite".to_string(),
            "prefer ripgrep over grep when searching".to_string(),
        ];
        let clusters = cluster_by_similarity(&summaries);
        // The two restatements share a cluster; the distinct lesson stands alone.
        assert_eq!(clusters.len(), 2, "got {clusters:?}");
        let sizes: Vec<usize> = clusters.iter().map(Vec::len).collect();
        assert!(
            sizes.contains(&2) && sizes.contains(&1),
            "expected one pair and one singleton, got {sizes:?}"
        );
    }

    #[test]
    fn an_empty_queue_clusters_to_nothing() {
        assert!(cluster_by_similarity(&[]).is_empty());
    }

    #[test]
    fn flag_unhelpful_routes_to_review_without_deleting() {
        // Outcome-aware down-weighting flags a lesson for review (never deletes it):
        // it appears in the review list and stays in accepted memory.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();
        let lesson = crate::SeedLesson {
            body: "a lesson that did not help on the eval".to_string(),
            category: Some("Process".to_string()),
            confidence: Some(0.8),
            related_files: Vec::new(),
            related_entities: Vec::new(),
            evidence: None,
            tags: Vec::new(),
        };
        crate::seed_memory(root, &[lesson], false).unwrap();
        let id = memory_list(root).unwrap()[0].id.clone();

        assert!(flag_unhelpful_lesson(root, &id).unwrap());
        assert!(
            lessons_flagged_for_review(root).unwrap().contains(&id),
            "the flagged lesson must surface in the review list"
        );
        assert!(
            memory_list(root).unwrap().iter().any(|m| m.id == id),
            "down-weighting must never delete the memory"
        );
    }

    #[test]
    fn context_hits_missing_config_is_empty_not_error() {
        // A project that has never closed out has no config and no store. That is
        // "nothing to inject", an empty result — never an error.
        let dir = tempfile::tempdir().unwrap();
        let hits = context_hits(dir.path(), "anything", None).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn context_hits_disabled_injection_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        crate::initialize(dir.path()).unwrap();
        memory_disable_injection(dir.path()).unwrap();
        let hits = context_hits(dir.path(), "anything", None).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn context_hits_learning_disabled_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(crate::CONFIG_FILE),
            "[learning]\nenabled = false\n",
        )
        .unwrap();
        let hits = context_hits(dir.path(), "anything", None).unwrap();
        assert!(hits.is_empty());
    }

    /// A fixture `/v1/embeddings` server whose vector depends on the input
    /// text: `cypress` and the plain query embed to `[0, 1]`, the `lesson`
    /// bodies without `cypress` to `[1, 0]` — so the semantically-closer
    /// memory is NOT the keyword-stronger one.
    fn content_aware_embeddings_server(max_requests: usize) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for _ in 0..max_requests {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let mut request = Vec::new();
                let mut buffer = [0_u8; 2048];
                loop {
                    match stream.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => {
                            request.extend_from_slice(&buffer[..read]);
                            let text = String::from_utf8_lossy(&request);
                            if text.contains("\r\n\r\n") && text.trim_end().ends_with('}') {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let text = String::from_utf8_lossy(&request);
                let vector = if text.contains("cypress") {
                    "[0.0,1.0]"
                } else if text.contains("lesson") {
                    "[1.0,0.0]"
                } else {
                    "[0.0,1.0]"
                };
                let body = format!("{{\"data\":[{{\"embedding\":{vector}}}]}}");
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{address}")
    }

    fn seed_two_memories(root: &Path) {
        for body in [
            "redwood task lesson about builds",
            "cypress task lesson about builds",
        ] {
            let lesson = crate::SeedLesson {
                body: body.to_string(),
                category: Some("Process".to_string()),
                confidence: Some(0.8),
                related_files: Vec::new(),
                related_entities: Vec::new(),
                evidence: None,
                tags: Vec::new(),
            };
            crate::seed_memory(root, &[lesson], false).unwrap();
        }
    }

    #[test]
    fn the_rerank_stage_reorders_injection_candidates_only_when_opted_in() {
        // Query "redwood task" keyword-prefers the redwood memory (two term
        // matches vs one); the stub embeddings make the cypress memory the
        // semantically closer one. Rerank off → keyword order. Rerank on →
        // the cypress memory climbs.
        let base_url = content_aware_embeddings_server(32);

        // Off (default posture): keyword order, byte-identical.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(crate::CONFIG_FILE),
            format!(
                "[learning]\nenabled = true\n\n[inference]\nembedding_base_url = \"{base_url}\"\nembedding_model = \"stub\"\ntimeout_secs = 5\n",
            ),
        )
        .unwrap();
        seed_two_memories(dir.path());
        let hits = context_hits(dir.path(), "redwood task", None).unwrap();
        assert!(hits.len() >= 2, "both memories must be candidates");
        assert!(
            hits[0].snippet.contains("redwood"),
            "rerank off: keyword order stands, got {:?}",
            hits.iter().map(|h| h.snippet.clone()).collect::<Vec<_>>()
        );

        // On: the stored-vector rerank reorders the top candidates.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(crate::CONFIG_FILE),
            format!(
                "[learning]\nenabled = true\n\n[retrieval]\nrerank = true\n\n[inference]\nembedding_base_url = \"{base_url}\"\nembedding_model = \"stub\"\ntimeout_secs = 5\n",
            ),
        )
        .unwrap();
        seed_two_memories(dir.path());
        let hits = context_hits(dir.path(), "redwood task", None).unwrap();
        assert!(hits.len() >= 2, "both memories must be candidates");
        assert!(
            hits[0].snippet.contains("cypress"),
            "rerank on: the semantically closer memory must climb, got {:?}",
            hits.iter().map(|h| h.snippet.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn context_hits_corrupt_store_errors_rather_than_masking() {
        // The smallest artefact proving the masking is gone: a configured,
        // learning-enabled project whose existing store file is unreadable must
        // surface an error, not collapse to "no memory".
        let dir = tempfile::tempdir().unwrap();
        crate::initialize(dir.path()).unwrap();
        let state = dir.path().join(".localmind");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(
            state.join("localmind.sqlite"),
            b"this is not a sqlite database",
        )
        .unwrap();

        let result = context_hits(dir.path(), "anything", None);
        assert!(
            matches!(result, Err(LearningError::Context(_))),
            "expected a propagated Context error, got {result:?}"
        );
    }
}
