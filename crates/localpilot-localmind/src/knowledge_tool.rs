//! A model-callable tool that searches the project's ingested knowledge base.
//!
//! This is the "pull" half of project knowledge: instead of always-on context
//! seeded into every turn, the model calls this tool to retrieve relevant
//! chunks from the deterministic, redacted index built by `localpilot ingest`.
//! It is read-only — it only reads the derived index under the project root —
//! so the permission engine auto-allows it like the other read tools.

use async_trait::async_trait;
use localpilot_sandbox::Effect;
use localpilot_tools::{Tool, ToolContext, ToolError, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::pack::PackSource;

/// Default number of hits returned when the caller does not ask for a count.
const DEFAULT_MAX_HITS: usize = 5;
/// Ceiling on hits, so a single call cannot flood the context.
const MAX_HITS: usize = 20;
/// Bound on each snippet, keeping the result lean.
const SNIPPET_CHARS: usize = 240;
/// Token budget for the ranked pack a single call computes.
const PACK_TOKEN_BUDGET: u64 = 2_048;
/// Minimum normalized relevance points an entry needs to appear in the
/// rendered window (unit relevance 0.05 on the pack's 200-point scale).
/// Manual pins are exempt — the user chose them.
const VISIBLE_RELEVANCE_FLOOR: i64 = 10;

/// A short, stable label for each pack source.
fn source_label(source: PackSource) -> &'static str {
    match source {
        PackSource::ManualPin => "pinned",
        PackSource::AcceptedMemory => "memory",
        PackSource::RecentSession => "recent session",
        PackSource::Ingest => "ingested file",
        PackSource::CodeGraph => "code graph",
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct KnowledgeSearchInput {
    /// What to look up in the project's ingested knowledge base.
    query: String,
    /// Maximum number of results to return (default 5, capped at 20).
    #[serde(default)]
    max_hits: Option<usize>,
}

/// Searches the project's ingested knowledge base for a query and returns ranked
/// `path:line` snippets. Read-only.
pub struct KnowledgeSearch;

#[async_trait]
impl Tool for KnowledgeSearch {
    fn name(&self) -> &str {
        "knowledge_search"
    }

    fn description(&self) -> &str {
        "Search the project's knowledge base for text relevant to a query, returning ranked \
         snippets across ingested files, accepted project memory, recent-session facts, and code \
         structure. Read-only. Use it to pull project facts on demand instead of relying on \
         always-on context."
    }

    fn schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(KnowledgeSearchInput)).unwrap_or(Value::Null)
    }

    fn approval_detail(&self, input: &Value) -> String {
        input
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .chars()
            .take(160)
            .collect()
    }

    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        // Only reads the derived index under the project root.
        Ok(vec![Effect::ReadPath {
            inside_workspace: true,
            secret_like: false,
        }])
    }

    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: KnowledgeSearchInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let limit = input
            .max_hits
            .unwrap_or(DEFAULT_MAX_HITS)
            .clamp(1, MAX_HITS);
        let root = ctx.workspace.root();

        // A missing index is normal (project not ingested yet) and is reported as
        // such before any query runs. A present-but-unreadable index is
        // distinguished so a corrupt store is visible rather than masked as "no
        // knowledge". Either way the turn never breaks on a knowledge miss.
        if !crate::ingest::has_chunk_index(root) {
            return Ok(ToolOutput::ok(
                "no indexed project knowledge yet (run `localpilot ingest` to build it)",
            ));
        }
        // Compute a ranked cross-source pack on demand (read-only). Exclude the
        // live/in-progress session so the current conversation is not served back
        // to itself as a "knowledge-base match".
        let exclude = crate::ingest::active_session(root);
        let pack = match crate::ingest::compute_pack(
            root,
            &input.query,
            PACK_TOKEN_BUDGET,
            exclude.as_deref(),
        ) {
            Ok(pack) => pack,
            Err(_) => {
                return Ok(ToolOutput::ok(
                    "project knowledge index is unreadable; rebuild it with \
                     `localpilot ingest rebuild`",
                ));
            }
        };
        if pack.entries.is_empty() {
            return Ok(ToolOutput::ok(format!(
                "no knowledge-base matches for \"{}\"",
                input.query
            )));
        }

        // The visible window is relevance-ordered over the *already-selected*
        // pack — allocation order is reserve/source order, so rendering the
        // first N entries verbatim would let a full reserve hide every
        // relevant hit from another source. Entries below the relevance floor
        // are not shown merely to fill `max_hits` (manual pins are user-chosen
        // and always shown); fewer than `max_hits` results — including zero —
        // is an honest answer. This reorders and filters the rendering only;
        // the pack's protected selection is untouched.
        let mut visible: Vec<_> = pack
            .entries
            .iter()
            .filter(|entry| {
                entry.source == PackSource::ManualPin
                    || entry.signals.relevance >= VISIBLE_RELEVANCE_FLOOR
            })
            .collect();
        visible.sort_by(|a, b| {
            b.signals
                .final_score
                .cmp(&a.signals.final_score)
                .then_with(|| a.id.cmp(&b.id))
        });
        if visible.is_empty() {
            return Ok(ToolOutput::ok(format!(
                "no knowledge-base matches for \"{}\" cleared the relevance floor \
                 ({} weaker candidates withheld)",
                input.query,
                pack.entries.len()
            )));
        }

        let mut out = format!("Knowledge-base matches for \"{}\":\n", input.query);
        for entry in visible.iter().take(limit) {
            let source = source_label(entry.source);
            let path = entry.path.as_deref().unwrap_or("(no path)");
            let stale = if entry.stale { " (stale)" } else { "" };
            let snippet: String = entry.snippet.chars().take(SNIPPET_CHARS).collect();
            out.push_str(&format!("- [{source}] {path}{stale} — {snippet}\n"));
        }
        Ok(ToolOutput::ok(out))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use localpilot_config::IngestConfig;
    use localpilot_sandbox::{Interactivity, Workspace};
    use serde_json::json;

    fn context(workspace: &Workspace) -> ToolContext<'_> {
        ToolContext {
            workspace,
            interactivity: Interactivity::NonInteractive,
            trusted: true,
            retention: None,
            processes: None,
        }
    }

    #[tokio::test]
    async fn returns_indexed_hits_for_a_query() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn distinctive_marker_symbol() -> u32 { 7 }\n",
        )
        .unwrap();
        crate::ingest::run(
            dir.path(),
            &IngestConfig::default(),
            crate::ingest::RunMode::Full,
        )
        .unwrap();

        let ws = Workspace::new(dir.path()).unwrap();
        let out = KnowledgeSearch
            .invoke(
                json!({ "query": "distinctive_marker_symbol" }),
                &context(&ws),
            )
            .await
            .unwrap();

        assert!(!out.is_error);
        assert!(
            out.text.contains("src/lib.rs"),
            "expected the indexed file in the result, got: {}",
            out.text
        );
    }

    #[tokio::test]
    async fn empty_index_is_a_useful_result_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();

        let out = KnowledgeSearch
            .invoke(json!({ "query": "anything" }), &context(&ws))
            .await
            .unwrap();

        assert!(!out.is_error, "a missing index must not be an error");
        assert!(out.text.contains("no indexed project knowledge"));
    }

    #[tokio::test]
    async fn a_corrupt_index_is_reported_distinctly_not_masked_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        // Present-but-unreadable store: distinct from "not indexed yet".
        std::fs::create_dir_all(dir.path().join(".localmind/ingest")).unwrap();
        std::fs::write(
            dir.path().join(".localmind/ingest/chunks.sqlite"),
            "this is not a sqlite database",
        )
        .unwrap();
        let ws = Workspace::new(dir.path()).unwrap();

        let out = KnowledgeSearch
            .invoke(json!({ "query": "anything" }), &context(&ws))
            .await
            .unwrap();

        assert!(!out.is_error, "a corrupt index must not break the turn");
        assert!(
            out.text.contains("unreadable"),
            "a corrupt index must be reported distinctly, got: {}",
            out.text
        );
    }

    #[tokio::test]
    async fn honors_the_max_hits_cap() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        // Several files all matching the same term.
        for i in 0..5 {
            std::fs::write(
                dir.path().join(format!("src/file{i}.rs")),
                "// shared_term shared_term shared_term\n",
            )
            .unwrap();
        }
        crate::ingest::run(
            dir.path(),
            &IngestConfig::default(),
            crate::ingest::RunMode::Full,
        )
        .unwrap();
        let ws = Workspace::new(dir.path()).unwrap();

        let out = KnowledgeSearch
            .invoke(
                json!({ "query": "shared_term", "max_hits": 2 }),
                &context(&ws),
            )
            .await
            .unwrap();

        let lines = out.text.lines().filter(|l| l.starts_with("- ")).count();
        assert_eq!(
            lines, 2,
            "result must respect the max_hits cap, got: {}",
            out.text
        );
    }

    fn seed_memory(root: &std::path::Path, id: &str, body: &str) {
        use localmind_core::{
            Confidence, EvidenceKind, EvidenceRef, LessonCategory, MemoryEntry, MemoryEntryId,
            MemoryScope, MemoryStatus, SyncMeta,
        };
        use localmind_store::MemoryPersistence;
        let entry = MemoryEntry {
            id: MemoryEntryId::new(id),
            scope: MemoryScope::Project,
            body: body.to_string(),
            category: LessonCategory::ProjectConvention,
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
            sync_meta: SyncMeta::default(),
        };
        MemoryPersistence::open_project(root)
            .unwrap()
            .persist_memory_entry(&entry)
            .unwrap();
    }

    #[tokio::test]
    async fn relevant_sources_stay_visible_while_weak_reserved_noise_is_withheld() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // A strong ingest hit for the query.
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/registry.rs"),
            "// the quokka registry mirror is pinned before publishing\n\
             pub fn quokka_registry_mirror() {}\n",
        )
        .unwrap();
        crate::ingest::run(root, &IngestConfig::default(), crate::ingest::RunMode::Full).unwrap();
        // Accepted memory: one relevant lesson plus a flood of weak
        // boilerplate dumps that technically match query terms but bury them
        // in chrome — the shape that used to fill the visible window.
        std::fs::write(
            root.join(".localmind.toml"),
            "[learning]\nenabled = true\nallowed_scopes = [\"project\"]\n",
        )
        .unwrap();
        seed_memory(
            root,
            "relevant-lesson",
            "Pin the quokka registry mirror before publishing packages.",
        );
        let chrome = "home pricing docs blog careers contact sign in get started ".repeat(60);
        for index in 0..6 {
            seed_memory(
                root,
                &format!("junk-{index}"),
                &format!("{chrome} registry {chrome} mirror {chrome} item {index}"),
            );
        }
        // An unrelated recent session that must contribute nothing.
        let session_dir = root.join(".localmind").join("sessions").join("s-old");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("summary.json"),
            r#"{"key_points":["load tailwind through a cdn link"]}"#,
        )
        .unwrap();

        let ws = Workspace::new(root).unwrap();
        let out = KnowledgeSearch
            .invoke(json!({ "query": "quokka registry mirror" }), &context(&ws))
            .await
            .unwrap();

        assert!(
            out.text.contains("src/registry.rs"),
            "the strong ingest hit must be visible: {}",
            out.text
        );
        assert!(
            out.text
                .contains("quokka registry mirror before publishing"),
            "the relevant accepted memory must be visible: {}",
            out.text
        );
        assert!(
            !out.text.contains("home pricing docs blog"),
            "weak boilerplate memories must not fill the visible window: {}",
            out.text
        );
        assert!(
            !out.text.contains("tailwind"),
            "an unrelated session fact must not appear: {}",
            out.text
        );
    }

    #[test]
    fn the_effect_is_a_read_inside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let effects = KnowledgeSearch
            .effects(&json!({ "query": "x" }), &context(&ws))
            .unwrap();
        assert_eq!(
            effects,
            vec![Effect::ReadPath {
                inside_workspace: true,
                secret_like: false
            }]
        );
    }
}
