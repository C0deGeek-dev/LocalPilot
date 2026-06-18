//! Pre-turn context hook: contribute relevant LocalMind knowledge to a turn.
//!
//! Only accepted, review-gated project memory is contributed as always-on
//! context (lean, and injected into the request rather than stored, so it never
//! accumulates). Ingested folder knowledge is reached on demand through the
//! `knowledge_search` tool instead of being seeded every turn — unless the
//! project opts back into the legacy push behavior via `[ingest] mode = "push"`.
//!
//! This lives in the engine crate (not the host binary) so the pull/push gate is
//! unit-testable; the host just registers it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use localpilot_config::{CliOverrides, ConfigPaths, IngestConfig, IngestMode};
use localpilot_harness::{ContextContribution, ContextHook, SessionRuntime};
use localpilot_store::MemoryUsed;

/// Cap on the accepted-memory block, so the always-on context stays lean
/// regardless of how large the memory store grows.
const ACCEPTED_MEMORY_CHAR_CAP: usize = 1_200;

/// Cap on the always-on repo-primer block, so session-start orientation stays
/// a small, bounded token cost.
const PRIMER_CHAR_CAP: usize = 1_000;

/// The audit id for the always-on repository primer block (it is one block, not
/// a searchable memory row).
const PRIMER_ID: &str = "<repository-primer>";

/// LocalMind retrieval as a pre-turn context hook. Best-effort — a miss or error
/// contributes nothing and never fails the turn.
pub struct LocalMindContext {
    root: PathBuf,
}

impl LocalMindContext {
    /// A hook rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn ingest_config(&self) -> Option<IngestConfig> {
        localpilot_config::load(&ConfigPaths::standard(&self.root), &CliOverrides::default())
            .ok()
            .map(|config| config.ingest)
    }
}

impl ContextHook for LocalMindContext {
    fn name(&self) -> &str {
        "localmind-context"
    }

    fn context_for(&self, prompt: &str) -> Option<String> {
        self.contribute(prompt).text
    }

    fn memories_used(&self, prompt: &str) -> Vec<MemoryUsed> {
        self.contribute(prompt).memories
    }

    /// The injected context and the exact memories it represents, from a single
    /// retrieval, so the "memories used" record matches the injection block for
    /// block. Each injected block contributes its records under its own layer
    /// (`primer`, `memory`, `ingest`); a memory whose snippet does not fit the
    /// char budget is neither injected nor recorded.
    fn contribute(&self, prompt: &str) -> ContextContribution {
        let mut blocks: Vec<String> = Vec::new();
        let mut memories: Vec<MemoryUsed> = Vec::new();

        // The accepted cold-start primer: always-on orientation (not prompt
        // relevance), token-bounded. An unaccepted or stale primer is not active.
        if let Some(text) = crate::primer::accepted_primer(&self.root).ok().flatten() {
            blocks.push(format!(
                "Repository primer:\n{}",
                bound(&text, PRIMER_CHAR_CAP)
            ));
            memories.push(MemoryUsed {
                id: PRIMER_ID.to_string(),
                score: 0,
                layer: "primer".to_string(),
            });
        }

        // Accepted memory: one ranked, capped retrieval feeds both the injected
        // block and the recorded set, line by line under the char budget.
        if let Ok(hits) = crate::ops::context_hits(&self.root, prompt) {
            let mut block = String::from("Relevant accepted project memory:\n");
            let mut wrote = false;
            for hit in hits {
                let line = format!("- {}\n", hit.snippet.trim());
                if block.chars().count() + line.chars().count() > ACCEPTED_MEMORY_CHAR_CAP {
                    break;
                }
                block.push_str(&line);
                wrote = true;
                memories.push(MemoryUsed {
                    id: hit.memory_id,
                    score: hit.score,
                    layer: "memory".to_string(),
                });
            }
            if wrote {
                blocks.push(block.trim_end().to_string());
            }
        }

        // Ingested knowledge only in legacy push mode; record the exact chunks.
        if let Some(config) = self.ingest_config() {
            if config.enabled && config.mode == IngestMode::Push {
                if let Ok(Some((text, ids))) =
                    crate::ingest::context_for_prompt_with_ids(&self.root, prompt)
                {
                    blocks.push(text.trim_end().to_string());
                    for id in ids {
                        memories.push(MemoryUsed {
                            id,
                            score: 0,
                            layer: "ingest".to_string(),
                        });
                    }
                }
            }
        }

        ContextContribution {
            text: (!blocks.is_empty()).then(|| blocks.join("\n")),
            memories,
        }
    }
}

/// Truncate `text` to at most `cap` characters, adding a marker when it was cut.
fn bound(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_string();
    }
    let truncated: String = text.chars().take(cap).collect();
    format!("{truncated}\n… (memory truncated)")
}

/// Register the LocalMind context hook on a session runtime.
pub fn register_context_hook(cwd: &Path, runtime: &mut SessionRuntime) {
    runtime
        .hooks_mut()
        .register_context_hook(Arc::new(LocalMindContext::new(cwd)));
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::ingest::{run as ingest_run, RunMode};
    use localpilot_config::IngestConfig;

    fn seed_ingest(root: &Path) {
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn distinctive_marker_symbol() -> u32 { 7 }\n",
        )
        .unwrap();
        ingest_run(root, &IngestConfig::default(), RunMode::Full).unwrap();
    }

    #[test]
    fn pull_mode_does_not_inject_ingested_knowledge() {
        let dir = tempfile::tempdir().unwrap();
        seed_ingest(dir.path());
        // No .localpilot.toml → default mode is pull, and there is no accepted
        // memory store, so the hook contributes nothing even though the ingest
        // index would match the prompt.
        let hook = LocalMindContext::new(dir.path());
        assert_eq!(hook.context_for("distinctive_marker_symbol"), None);
    }

    #[test]
    fn push_mode_injects_ingested_knowledge() {
        let dir = tempfile::tempdir().unwrap();
        seed_ingest(dir.path());
        std::fs::write(
            dir.path().join(".localpilot.toml"),
            "[ingest]\nenabled = true\nmode = \"push\"\n",
        )
        .unwrap();
        let hook = LocalMindContext::new(dir.path());
        let context = hook
            .context_for("distinctive_marker_symbol")
            .expect("push mode must inject the matching ingest chunk");
        assert!(
            context.contains("src/lib.rs"),
            "expected the ingested file in the pushed context, got: {context}"
        );
    }

    #[test]
    fn an_accepted_primer_is_injected_into_session_context() {
        use localmind_core::{ReviewAction, ReviewDecision, ReviewItemId};
        use localmind_store::{MemoryPersistence, ReviewQueue};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn hub() -> u8 { 1 }\nfn caller() { hub(); }\n",
        )
        .unwrap();
        crate::codegraph_reindex(root, usize::MAX).unwrap();
        let id = crate::distill_primer_into_review(root).unwrap().unwrap();

        // Before acceptance the hook injects nothing.
        let hook = LocalMindContext::new(root);
        assert_eq!(hook.context_for("anything"), None);

        // Accept + promote the primer, then the hook includes it always-on.
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

        let context = hook
            .context_for("a prompt unrelated to the primer text")
            .expect("the accepted primer is always-on context");
        assert!(context.contains("Repository primer:"));

        // The injected primer block is recorded in the audit under its own layer,
        // so the inspector reflects what actually rode in the turn's context.
        let used = hook.memories_used("a prompt unrelated to the primer text");
        assert!(
            used.iter().any(|m| m.layer == "primer"),
            "the injected primer must be recorded with the primer layer: {used:?}"
        );
    }

    #[test]
    fn memories_used_reports_a_relevant_accepted_memory() {
        use localmind_core::{
            Confidence, EvidenceKind, EvidenceRef, LessonCategory, MemoryEntry, MemoryEntryId,
            MemoryScope, MemoryStatus,
        };
        use localmind_store::MemoryPersistence;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();
        let entry = MemoryEntry {
            id: MemoryEntryId::new("mem-redact"),
            scope: MemoryScope::Project,
            body: "always redact secrets before persisting a transcript".to_string(),
            category: LessonCategory::SecurityWarning,
            confidence: Confidence::new(0.9).unwrap(),
            source_session: None,
            evidence: vec![EvidenceRef::new(EvidenceKind::ManualNote, "seeded")],
            tags: Vec::new(),
            related_files: Vec::new(),
            related_entities: Vec::new(),
            created_at: None,
            updated_at: None,
            supersedes: Vec::new(),
            contradicts: Vec::new(),
            status: MemoryStatus::Active,
        };
        MemoryPersistence::open_project(root)
            .unwrap()
            .persist_memory_entry(&entry)
            .unwrap();

        let hook = LocalMindContext::new(root);
        let used = hook.memories_used("how should I redact secrets");
        assert!(
            used.iter()
                .any(|m| m.id == "mem-redact" && m.layer == "memory"),
            "the relevant accepted memory must be reported as used: {used:?}"
        );

        // An unrelated prompt surfaces nothing.
        assert!(hook.memories_used("audio playback latency").is_empty());
    }

    #[test]
    fn bound_truncates_with_a_marker() {
        let long = "x".repeat(2_000);
        let bounded = bound(&long, 1_200);
        // Capped near the limit: 1200 kept chars plus a short truncation marker,
        // well under the un-truncated 2000.
        assert!(bounded.chars().count() < 1_300);
        assert!(bounded.starts_with(&"x".repeat(1_200)));
        assert!(bounded.contains("memory truncated"));
        // Short input is returned unchanged.
        assert_eq!(bound("short", 1_200), "short");
    }

    fn seed_memory(root: &Path, id: &str, body: &str) {
        use localmind_core::{
            Confidence, EvidenceKind, EvidenceRef, LessonCategory, MemoryEntry, MemoryEntryId,
            MemoryScope, MemoryStatus,
        };
        use localmind_store::MemoryPersistence;
        let entry = MemoryEntry {
            id: MemoryEntryId::new(id),
            scope: MemoryScope::Project,
            body: body.to_string(),
            category: LessonCategory::SecurityWarning,
            confidence: Confidence::new(0.9).unwrap(),
            source_session: None,
            evidence: vec![EvidenceRef::new(EvidenceKind::ManualNote, "seeded")],
            tags: Vec::new(),
            related_files: Vec::new(),
            related_entities: Vec::new(),
            created_at: None,
            updated_at: None,
            supersedes: Vec::new(),
            contradicts: Vec::new(),
            status: MemoryStatus::Active,
        };
        MemoryPersistence::open_project(root)
            .unwrap()
            .persist_memory_entry(&entry)
            .unwrap();
    }

    #[test]
    fn memories_used_is_capped_to_the_injected_set() {
        // More matches than are injected: the audit records at most the injected
        // cap, never the full result set (the over-report this fix closes).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();
        for i in 0..8 {
            seed_memory(
                root,
                &format!("mem-{i}"),
                &format!("widget pipeline note number {i}"),
            );
        }
        let hook = LocalMindContext::new(root);
        let used = hook.memories_used("widget pipeline note");
        let memory_layer = used.iter().filter(|m| m.layer == "memory").count();
        assert!(memory_layer > 0, "expected matches to be recorded");
        assert!(
            memory_layer <= crate::ops::CONTEXT_MEMORY_LIMIT,
            "audit must not exceed the injected cap: {memory_layer}"
        );
    }

    #[test]
    fn the_audit_records_exactly_the_injected_memory_lines() {
        // The audit and the injection come from one retrieval under one budget,
        // so the recorded memory-layer entries equal the injected `- ` lines
        // exactly — never a memory that was not injected, nor one omitted.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();
        for i in 0..4 {
            seed_memory(
                root,
                &format!("note-{i}"),
                &format!("widget pipeline note number {i}"),
            );
        }
        let hook = LocalMindContext::new(root);
        let contribution = hook.contribute("widget pipeline note");
        let recorded = contribution
            .memories
            .iter()
            .filter(|m| m.layer == "memory")
            .count();
        let text = contribution.text.unwrap_or_default();
        let injected_lines = text.lines().filter(|line| line.starts_with("- ")).count();
        assert!(recorded >= 1, "expected matches");
        assert_eq!(
            recorded, injected_lines,
            "every recorded memory is an injected line and vice versa"
        );
    }
}
