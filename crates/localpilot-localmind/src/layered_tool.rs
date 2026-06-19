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
        let root = ctx.workspace.root();
        let expansions = crate::layered::expand_layer(root, &ids)
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
        let root = ctx.workspace.root();
        let bodies = crate::layered::fetch_layer(root, &ids)
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        if bodies.is_empty() {
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
        let index = crate::layered::index_layer(dir.path(), "marker_widget", 5).unwrap();
        let id = index[0].id.clone();

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
        let index = crate::layered::index_layer(dir.path(), "marker_widget", 5).unwrap();
        let out = KnowledgeExpand
            .invoke(json!({ "ids": [index[0].id.clone()] }), &context(&ws))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.text.contains("expand layer"));
    }
}
