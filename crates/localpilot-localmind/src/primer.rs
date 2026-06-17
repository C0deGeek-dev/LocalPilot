//! Cold-start repo primer: host driver and session-start injection.
//!
//! The engine distils the primer (deterministic, from the code-graph overview);
//! the host decides *when* to distil it (at the end of a successful index, under
//! the project's learning gate) and surfaces the **accepted** one at session
//! start. Distillation only enqueues a review candidate — acceptance is the
//! reviewer's call — so an unreviewed or stale primer is never injected.

use crate::LearningError;
use localmind_codegraph::{compute_overview, distill_primer, OverviewOptions};
use localmind_core::SessionId;
use localmind_store::{GraphStore, MemoryPersistence, ProjectConfig, ReviewQueue};
use std::path::Path;

/// Stable id prefix the engine stamps on every primer candidate, so the host can
/// recognize an accepted primer among project memory.
const PRIMER_ID_PREFIX: &str = "repo-primer-";

/// The session id distillation enqueues primer candidates under.
const PRIMER_SESSION: &str = "codegraph-primer";

/// Distils a primer from the current code graph and enqueues it for review.
/// Returns the enqueued candidate id, or `None` when there is nothing to do:
/// learning is disabled (trust gate), the graph is empty, or the current
/// structure's primer is already accepted (no drift). Best-effort; reuses the
/// existing index lifecycle rather than adding a new trigger.
pub fn distill_primer_into_review(project_root: &Path) -> Result<Option<String>, LearningError> {
    // Trust gate: `discover` itself errors when a project has learning disabled,
    // so any discovery failure (disabled, or no learning config) is a clean skip.
    let Ok(config) = ProjectConfig::discover(project_root) else {
        return Ok(None);
    };
    if !config.config.learning.enabled {
        return Ok(None);
    }

    let store = GraphStore::open_project(project_root)
        .map_err(|error| LearningError::Graph(error.to_string()))?;
    let overview = compute_overview(&store, OverviewOptions::default())
        .map_err(|error| LearningError::Graph(error.to_string()))?;
    if overview.file_count == 0 {
        return Ok(None);
    }

    let repo = project_root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".to_string());
    let commit = git_commit(project_root).unwrap_or_else(|| "working".to_string());
    let candidate = distill_primer(&overview, &repo, &commit)
        .map_err(|error| LearningError::Graph(error.to_string()))?;

    // If this exact primer is already accepted, the structure has not drifted —
    // nothing to enqueue. A drifted repo yields a new id the reviewer supersedes.
    if accepted_primer_id(project_root)?.as_deref() == Some(candidate.id.as_str()) {
        return Ok(None);
    }

    let queue = ReviewQueue::open_project(project_root)
        .map_err(|error| LearningError::Review(error.to_string()))?;
    let inserted = queue
        .enqueue_candidates(
            &SessionId::new(PRIMER_SESSION),
            std::slice::from_ref(&candidate),
        )
        .map_err(|error| LearningError::Review(error.to_string()))?;
    if inserted == 0 {
        // Already pending (deduped at enqueue); not newly enqueued.
        return Ok(None);
    }
    Ok(Some(candidate.id.as_str().to_string()))
}

/// The body of the accepted repo primer for this project, if one exists and
/// injection is enabled. Only active (accepted, non-superseded) memory is
/// returned, so an unaccepted or stale primer is never surfaced.
pub fn accepted_primer(project_root: &Path) -> Result<Option<String>, LearningError> {
    if !crate::ops::memory_injection_enabled(project_root) {
        return Ok(None);
    }
    let memory = match MemoryPersistence::open_project(project_root) {
        Ok(memory) => memory,
        Err(_) => return Ok(None),
    };
    let body = memory
        .list_memory()
        .map_err(|error| LearningError::Memory(error.to_string()))?
        .into_iter()
        .find(|record| record.memory_id.as_str().starts_with(PRIMER_ID_PREFIX))
        .map(|record| record.body);
    Ok(body)
}

/// The active primer's memory id, if one is accepted.
fn accepted_primer_id(project_root: &Path) -> Result<Option<String>, LearningError> {
    let memory = match MemoryPersistence::open_project(project_root) {
        Ok(memory) => memory,
        Err(_) => return Ok(None),
    };
    Ok(memory
        .list_memory()
        .map_err(|error| LearningError::Memory(error.to_string()))?
        .into_iter()
        .find(|record| record.memory_id.as_str().starts_with(PRIMER_ID_PREFIX))
        .map(|record| record.memory_id.as_str().to_string()))
}

/// Best-effort short git commit for `repo@commit` provenance; `None` outside a
/// git work tree or when git is unavailable.
fn git_commit(project_root: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(project_root)
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let commit = String::from_utf8(output.stdout).ok()?;
    let commit = commit.trim();
    if commit.is_empty() {
        None
    } else {
        Some(commit.to_string())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{accepted_primer, distill_primer_into_review};
    use crate::{codegraph_reindex, has_chunk_index, ingest_run, RunMode};
    use localmind_core::{NodeKind, ReviewAction, ReviewDecision, ReviewItemId};
    use localmind_store::{GraphStore, MemoryPersistence, ReviewQueue};
    use localpilot_config::IngestConfig;
    use std::fs;
    use std::path::Path;

    fn mixed_project(root: &Path, enabled: bool) {
        fs::write(
            root.join(".localmind.toml"),
            format!("[learning]\nenabled = {enabled}\n"),
        )
        .unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("app")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub fn hub() -> u8 { 1 }\nfn caller() { hub(); }\n",
        )
        .unwrap();
        fs::write(
            root.join("app/main.py"),
            "def start():\n    return helper()\n\ndef helper():\n    return 1\n",
        )
        .unwrap();
    }

    #[test]
    fn one_index_pass_produces_graph_nodes_and_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        mixed_project(root, true);

        // Code files (every supported language) → the code graph.
        codegraph_reindex(root, usize::MAX).unwrap();
        let store = GraphStore::open_project(root).unwrap();
        let functions: Vec<String> = store
            .nodes_by_kind(NodeKind::Function)
            .unwrap()
            .into_iter()
            .map(|node| node.qualified_name)
            .collect();
        assert!(
            functions.iter().any(|name| name.ends_with("::hub")),
            "rust symbol must be in the graph: {functions:?}"
        );
        assert!(
            functions.iter().any(|name| name.ends_with("::helper")),
            "python symbol must be in the graph: {functions:?}"
        );

        // Text/docs → the chunk store (RAG), same project.
        ingest_run(root, &IngestConfig::default(), RunMode::Full).unwrap();
        assert!(has_chunk_index(root), "the chunk index must be built");
    }

    #[test]
    fn distillation_enqueues_a_primer_and_honours_the_learning_gate() {
        // Learning disabled → no primer (the trust gate skips before indexing).
        let off = tempfile::tempdir().unwrap();
        mixed_project(off.path(), false);
        assert!(distill_primer_into_review(off.path()).unwrap().is_none());

        // Learning enabled → distillation enqueues a pending primer candidate.
        let on = tempfile::tempdir().unwrap();
        mixed_project(on.path(), true);
        codegraph_reindex(on.path(), usize::MAX).unwrap();
        let id = distill_primer_into_review(on.path())
            .unwrap()
            .expect("a primer candidate must be enqueued");
        assert!(id.starts_with("repo-primer-"));
        let queue = ReviewQueue::open_project(on.path()).unwrap();
        assert!(queue.summary().unwrap().pending >= 1);
    }

    #[test]
    fn only_an_accepted_primer_is_injected() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        mixed_project(root, true);
        codegraph_reindex(root, usize::MAX).unwrap();
        let id = distill_primer_into_review(root).unwrap().unwrap();

        // Enqueued but unreviewed → nothing injected.
        assert!(accepted_primer(root).unwrap().is_none());

        // Accept and promote it → now it is injected at session start.
        let queue = ReviewQueue::open_project(root).unwrap();
        let item = ReviewItemId::new(&id);
        queue
            .decide(ReviewDecision {
                item_id: item.clone(),
                action: ReviewAction::Accept,
                reviewer: "tester".to_string(),
                decided_at: None,
                note: None,
                replacement_summary: None,
                evidence: Vec::new(),
            })
            .unwrap();
        MemoryPersistence::open_project(root)
            .unwrap()
            .promote_review_item(&item)
            .unwrap();

        let primer = accepted_primer(root)
            .unwrap()
            .expect("an accepted primer must be injected");
        assert!(primer.contains("files"), "primer body: {primer}");
    }
}
