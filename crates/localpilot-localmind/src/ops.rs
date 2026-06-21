//! Review-queue, memory, and audit operations over the LocalMind store.
//!
//! These wrap LocalMind's project store and return plain LocalPilot-owned types
//! so callers (the CLI) never name a LocalMind type directly.

use localmind_core::{MemoryEntryId, ReviewAction, ReviewDecision, ReviewItemId, SkillDraftId};
use localmind_store::{
    MemoryPersistence, MemoryPersistenceError, MemoryRecord, ReviewQueue, ReviewQueueItem,
    SkillDraftRecord, SkillDraftStore, StoreConfigError,
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
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub memory_id: String,
    pub score: i64,
    pub path: String,
    pub snippet: String,
}

/// An accepted LocalMind memory entry, flattened for display.
#[derive(Debug, Clone)]
pub struct MemorySummary {
    pub id: String,
    pub scope: String,
    pub category: String,
    pub status: String,
    pub path: String,
    pub body: String,
}

fn memory_summary(record: MemoryRecord) -> MemorySummary {
    MemorySummary {
        id: record.memory_id.to_string(),
        scope: record.scope,
        category: record.category,
        status: record.status,
        path: record.path.display().to_string(),
        body: record.body,
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
pub fn context_hits(project_root: &Path, query: &str) -> Result<Vec<SearchHit>, LearningError> {
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
        .search(query)
        .map_err(|e| LearningError::Context(e.to_string()))?;
    Ok(hits
        .into_iter()
        .take(CONTEXT_MEMORY_LIMIT)
        .map(|hit| SearchHit {
            memory_id: hit.memory_id.to_string(),
            score: hit.score,
            path: hit.path.display().to_string(),
            snippet: hit.snippet,
        })
        .collect())
}

/// The number of accepted-memory hits injected into a turn's context — the cap
/// the audit record must share so it never lists a memory that was not injected.
pub const CONTEXT_MEMORY_LIMIT: usize = 5;

/// List accepted LocalMind memory.
///
/// # Errors
/// Returns [`LearningError::Memory`] if the memory index cannot be read.
pub fn memory_list(project_root: &Path) -> Result<Vec<MemorySummary>, LearningError> {
    let persistence = open_memory(project_root)?;
    let records = persistence.list_memory().map_err(memory_err)?;
    Ok(records.into_iter().map(memory_summary).collect())
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
    let hits = context_hits(project_root, query)?;
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
    fn context_hits_missing_config_is_empty_not_error() {
        // A project that has never closed out has no config and no store. That is
        // "nothing to inject", an empty result — never an error.
        let dir = tempfile::tempdir().unwrap();
        let hits = context_hits(dir.path(), "anything").unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn context_hits_disabled_injection_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        crate::initialize(dir.path()).unwrap();
        memory_disable_injection(dir.path()).unwrap();
        let hits = context_hits(dir.path(), "anything").unwrap();
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
        let hits = context_hits(dir.path(), "anything").unwrap();
        assert!(hits.is_empty());
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

        let result = context_hits(dir.path(), "anything");
        assert!(
            matches!(result, Err(LearningError::Context(_))),
            "expected a propagated Context error, got {result:?}"
        );
    }
}
