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

use crate::ingest::IngestError;
use crate::pack::PackSource;

/// Default number of hits returned when the caller does not ask for a count.
const DEFAULT_MAX_HITS: usize = 5;
/// Ceiling on hits, so a single call cannot flood the context.
const MAX_HITS: usize = 20;
/// Bound on each snippet, keeping the result lean.
const SNIPPET_CHARS: usize = 240;
/// Token budget for the ranked pack a single call computes.
const PACK_TOKEN_BUDGET: u64 = 2_048;

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

        // Compute a ranked cross-source pack on demand (read-only). A missing
        // index is normal (project not ingested yet); a present-but-unreadable
        // index is distinguished so a corrupt store is visible rather than masked
        // as "no knowledge". Either way the turn never breaks on a knowledge miss.
        // Exclude the live/in-progress session so the current conversation is not
        // served back to itself as a "knowledge-base match".
        let exclude = crate::ingest::active_session(root);
        let pack = match crate::ingest::compute_pack(
            root,
            &input.query,
            PACK_TOKEN_BUDGET,
            exclude.as_deref(),
        ) {
            Ok(pack) => pack,
            Err(IngestError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(ToolOutput::ok(
                    "no indexed project knowledge yet (run `localpilot ingest` to build it)",
                ));
            }
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

        let mut out = format!("Knowledge-base matches for \"{}\":\n", input.query);
        for entry in pack.entries.iter().take(limit) {
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
        // Present-but-unreadable index: distinct from "not indexed yet".
        std::fs::create_dir_all(dir.path().join(".localmind/ingest")).unwrap();
        std::fs::write(
            dir.path().join(".localmind/ingest/chunks.json"),
            "{ this is not valid json",
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
