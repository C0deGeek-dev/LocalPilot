//! Model-callable tools for the expand and fetch layers of layered retrieval.
//!
//! `knowledge_search` is the cheap index layer (ranked locators). These two add
//! the next two layers so the model can spend tokens deliberately: expand around
//! chosen ids to find neighbours, then fetch full bodies for only the ids it
//! decides are worth the cost. Both are read-only over the derived index, so the
//! permission engine auto-allows them like the other read tools. Each reports the
//! token cost it spent, so the budget stays visible.

use async_trait::async_trait;
use localpilot_sandbox::Effect;
use localpilot_tools::{Tool, ToolContext, ToolError, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

/// Cap on ids accepted in one call, so a single fetch cannot flood the context.
const MAX_IDS: usize = 20;
/// Bound on each fetched body rendered into the result.
const BODY_CHARS: usize = 1_200;

#[derive(Debug, Deserialize, JsonSchema)]
struct IdsInput {
    /// Chunk ids (from `knowledge_search`) to act on. Capped at 20.
    ids: Vec<String>,
}

fn read_effect() -> Vec<Effect> {
    vec![Effect::ReadPath {
        inside_workspace: true,
        secret_like: false,
    }]
}

fn capped_ids(input: Value) -> Result<Vec<String>, ToolError> {
    let input: IdsInput =
        serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
    Ok(input.ids.into_iter().take(MAX_IDS).collect())
}

/// The non-ingest id namespaces `knowledge_search` can emit. Only ingest chunk
/// ids are fetchable; these ids name content that lives elsewhere, and the
/// follow-up tools say so explicitly instead of returning a silent miss.
fn non_fetchable_reason(id: &str) -> Option<&'static str> {
    if id.starts_with("memory:") {
        Some("an accepted-memory entry — read it through the memory surfaces (memory search / the review UI), not the chunk fetch layer")
    } else if id.starts_with("graph:") {
        Some("a code-graph row — inspect the symbol through the code-graph surfaces, not the chunk fetch layer")
    } else if id.starts_with("session:") {
        Some("a recent-session fact — its snippet is already its full content")
    } else {
        None
    }
}

/// Split requested ids into fetchable chunk ids and explained rejections.
fn partition_fetchable(ids: Vec<String>) -> (Vec<String>, Vec<(String, &'static str)>) {
    let mut fetchable = Vec::new();
    let mut rejected = Vec::new();
    for id in ids {
        match non_fetchable_reason(&id) {
            Some(reason) => rejected.push((id, reason)),
            None => fetchable.push(id),
        }
    }
    (fetchable, rejected)
}

/// Expand chosen ids into their document neighbours (layer 2). Read-only.
pub struct KnowledgeExpand;

#[async_trait]
impl Tool for KnowledgeExpand {
    fn name(&self) -> &str {
        "knowledge_expand"
    }

    fn description(&self) -> &str {
        "Expand knowledge-base chunk ids (from knowledge_search) into their document neighbours — \
         the other chunks of the same file — so you can locate adjacent context before fetching \
         full bodies. Read-only and cheap; returns neighbour ids, not bodies."
    }

    fn schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(IdsInput)).unwrap_or(Value::Null)
    }

    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(read_effect())
    }

    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let ids = capped_ids(input)?;
        if ids.is_empty() {
            return Ok(ToolOutput::ok("no ids given to expand"));
        }
        let (fetchable, rejected) = partition_fetchable(ids);
        let root = ctx.workspace.root();
        let expansions = crate::layered::expand_layer(root, &fetchable)
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        let total_cost: u64 = expansions
            .iter()
            .map(|expansion| expansion.token_cost)
            .sum();
        let mut out = format!("Neighbours (expand layer, ~{total_cost} tokens):\n");
        for expansion in expansions {
            if expansion.neighbor_ids.is_empty() {
                out.push_str(&format!("- {}: (no document neighbours)\n", expansion.id));
            } else {
                out.push_str(&format!(
                    "- {}: {}\n",
                    expansion.id,
                    expansion.neighbor_ids.join(", ")
                ));
            }
        }
        for (id, reason) in rejected {
            out.push_str(&format!("- {id}: not fetchable — {reason}\n"));
        }
        Ok(ToolOutput::ok(out))
    }
}

/// Fetch full bodies for explicit ids (layer 3). Read-only.
pub struct KnowledgeFetch;

#[async_trait]
impl Tool for KnowledgeFetch {
    fn name(&self) -> &str {
        "knowledge_fetch"
    }

    fn description(&self) -> &str {
        "Fetch the full bodies of specific knowledge-base chunk ids (from knowledge_search or \
         knowledge_expand). This is the only expensive retrieval layer — call it once you know \
         which ids are worth the tokens. Read-only; returns only the ids you ask for."
    }

    fn schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(IdsInput)).unwrap_or(Value::Null)
    }

    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(read_effect())
    }

    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let ids = capped_ids(input)?;
        if ids.is_empty() {
            return Ok(ToolOutput::ok("no ids given to fetch"));
        }
        let (fetchable, rejected) = partition_fetchable(ids);
        let root = ctx.workspace.root();
        let bodies = crate::layered::fetch_layer(root, &fetchable)
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        if bodies.is_empty() && rejected.is_empty() {
            return Ok(ToolOutput::ok("no chunks matched the requested ids"));
        }
        let total_cost: u64 = bodies.iter().map(|body| body.token_cost).sum();
        let mut out = format!("Fetched bodies (fetch layer, ~{total_cost} tokens):\n");
        for body in bodies {
            let text: String = body.body.chars().take(BODY_CHARS).collect();
            out.push_str(&format!(
                "\n## {}:{}-{} [{}]\n{}\n",
                body.path, body.start_line, body.end_line, body.id, text
            ));
        }
        for (id, reason) in rejected {
            out.push_str(&format!("\n## {id}\nnot fetchable — {reason}\n"));
        }
        Ok(ToolOutput::ok(out))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

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

    fn ingested() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "// marker_widget module\nfn marker_widget() -> u32 { 7 }\n",
        )
        .unwrap();
        crate::ingest::run(
            dir.path(),
            &IngestConfig::default(),
            crate::ingest::RunMode::Full,
        )
        .unwrap();
        dir
    }

    #[tokio::test]
    async fn fetch_returns_the_requested_body() {
        let dir = ingested();
        let ws = Workspace::new(dir.path()).unwrap();
        let hits = crate::ingest::search(dir.path(), "marker_widget").unwrap();
        let id = hits[0].chunk_id.clone();

        let out = KnowledgeFetch
            .invoke(json!({ "ids": [id] }), &context(&ws))
            .await
            .unwrap();

        assert!(!out.is_error);
        assert!(out.text.contains("marker_widget"), "got: {}", out.text);
        assert!(out.text.contains("fetch layer"));
    }

    #[tokio::test]
    async fn empty_ids_are_a_useful_result_not_an_error() {
        let dir = ingested();
        let ws = Workspace::new(dir.path()).unwrap();
        let out = KnowledgeFetch
            .invoke(json!({ "ids": [] }), &context(&ws))
            .await
            .unwrap();
        assert!(!out.is_error);
    }

    #[tokio::test]
    async fn expand_lists_document_neighbours() {
        let dir = ingested();
        let ws = Workspace::new(dir.path()).unwrap();
        let hits = crate::ingest::search(dir.path(), "marker_widget").unwrap();
        let out = KnowledgeExpand
            .invoke(json!({ "ids": [hits[0].chunk_id.clone()] }), &context(&ws))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.text.contains("expand layer"));
    }

    #[tokio::test]
    async fn a_search_emitted_id_round_trips_into_fetch() {
        let dir = ingested();
        let ws = Workspace::new(dir.path()).unwrap();

        // The first layer emits the locator; the id printed there is the very
        // string the fetch layer accepts — the contract the tools document.
        let search = crate::KnowledgeSearch
            .invoke(json!({ "query": "marker_widget" }), &context(&ws))
            .await
            .unwrap();
        assert!(!search.is_error);
        let id_start = search
            .text
            .find("(id ")
            .expect("search output carries an id locator")
            + 4;
        let id_end = id_start
            + search.text[id_start..]
                .find(',')
                .expect("locator fields are comma-separated");
        let id = search.text[id_start..id_end].to_string();
        assert!(
            search.text.contains("fetchable"),
            "locator must state fetchability: {}",
            search.text
        );

        let fetched = KnowledgeFetch
            .invoke(json!({ "ids": [id] }), &context(&ws))
            .await
            .unwrap();
        assert!(!fetched.is_error);
        assert!(
            fetched.text.contains("marker_widget"),
            "the emitted id must fetch its full body: {}",
            fetched.text
        );
    }

    #[tokio::test]
    async fn non_ingest_ids_are_rejected_with_an_explanation() {
        let dir = ingested();
        let ws = Workspace::new(dir.path()).unwrap();

        let fetched = KnowledgeFetch
            .invoke(
                json!({ "ids": ["memory:abc123", "session:0"] }),
                &context(&ws),
            )
            .await
            .unwrap();
        assert!(!fetched.is_error);
        assert!(
            fetched.text.contains("memory:abc123")
                && fetched.text.contains("not fetchable")
                && fetched.text.contains("accepted-memory"),
            "a memory id must be rejected with its reason: {}",
            fetched.text
        );
        assert!(
            fetched.text.contains("session:0") && fetched.text.contains("already its full content"),
            "a session id must be rejected with its reason: {}",
            fetched.text
        );

        let expanded = KnowledgeExpand
            .invoke(json!({ "ids": ["graph:Symbol"] }), &context(&ws))
            .await
            .unwrap();
        assert!(!expanded.is_error);
        assert!(
            expanded.text.contains("graph:Symbol") && expanded.text.contains("not fetchable"),
            "a graph id must be rejected with its reason: {}",
            expanded.text
        );
    }
}
