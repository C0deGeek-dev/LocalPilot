//! A model-callable tool that searches the project's accepted LocalMind memory.
//!
//! `knowledge_search` ranks a cross-source pack (ingested files, recent sessions,
//! code graph) for a task; this tool is the narrow counterpart that searches only
//! *accepted* — human-reviewed, promoted — project memory, so the agent can check
//! what durable facts the project already holds. Read-only: it reads the memory
//! index and never writes, accepts, or promotes anything.

use async_trait::async_trait;
use localpilot_sandbox::Effect;
use localpilot_tools::{Tool, ToolContext, ToolError, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::fmt::Write as _;

/// Default number of hits returned when the caller does not ask for a count.
const DEFAULT_MAX_HITS: usize = 5;
/// Ceiling on hits, so a single call cannot flood the context.
const MAX_HITS: usize = 20;
/// Bound on each snippet, keeping the result lean.
const SNIPPET_CHARS: usize = 240;

#[derive(Debug, Deserialize, JsonSchema)]
struct MemorySearchInput {
    /// What to look up in the project's accepted memory.
    query: String,
    /// Maximum number of results to return (default 5, capped at 20).
    #[serde(default)]
    max_hits: Option<usize>,
}

/// Searches accepted (human-reviewed) project memory and returns ranked
/// `path`/snippet hits. Read-only.
pub struct MemorySearch;

#[async_trait]
impl Tool for MemorySearch {
    fn name(&self) -> &str {
        "localmind_memory_search"
    }

    fn description(&self) -> &str {
        "Search the project's accepted LocalMind memory — durable facts a human reviewed and \
         promoted — for a query, returning ranked path/snippet hits. Read-only. Use it to check \
         what the project already knows before acting; unlike `knowledge_search` it searches only \
         accepted memory, not ingested files or session history."
    }

    fn schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(MemorySearchInput)).unwrap_or(Value::Null)
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
        // Only reads the project-local accepted-memory index.
        Ok(vec![Effect::ReadPath {
            inside_workspace: true,
            secret_like: false,
        }])
    }

    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: MemorySearchInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let query = input.query.trim();
        if query.is_empty() {
            return Err(ToolError::InvalidInput(
                "query must not be empty".to_string(),
            ));
        }
        let limit = input
            .max_hits
            .unwrap_or(DEFAULT_MAX_HITS)
            .clamp(1, MAX_HITS);
        let root = ctx.workspace.root();

        let hits =
            match crate::ops::search_readonly(root, query) {
                Ok(hits) => hits,
                Err(_) => return Ok(ToolOutput::ok(
                    "accepted memory is unreadable; inspect it with `localpilot learning search`",
                )),
            };
        if hits.is_empty() {
            return Ok(ToolOutput::ok(format!(
                "no accepted memory matches \"{query}\" yet (a human promotes reviewed lessons \
                 into memory)"
            )));
        }

        let mut out = format!("Accepted memory matches for \"{query}\":\n");
        for hit in hits.iter().take(limit) {
            let snippet: String = hit.snippet.chars().take(SNIPPET_CHARS).collect();
            let _ = writeln!(out, "- {} — {}", hit.path, snippet.trim());
        }
        Ok(ToolOutput::ok(out))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::ops::ReviewVerdict;
    use localmind_core::{
        CandidateLesson, Confidence, LessonCategory, LessonId, SessionId as LearningSessionId,
        SuggestedAction,
    };
    use localmind_store::ReviewQueue;
    use localpilot_sandbox::{Interactivity, Workspace};
    use serde_json::json;
    use std::path::Path;

    fn context(workspace: &Workspace) -> ToolContext<'_> {
        ToolContext {
            workspace,
            interactivity: Interactivity::NonInteractive,
            trusted: true,
            retention: None,
        }
    }

    /// Enqueue, accept, and promote one lesson so it becomes accepted memory.
    fn seed_accepted_memory(root: &Path, summary: &str) {
        crate::initialize(root).unwrap();
        let candidate = CandidateLesson::new(
            LessonId::new("seed-memory"),
            summary.to_string(),
            LessonCategory::ProjectConvention,
            Confidence::new(0.9).unwrap(),
            SuggestedAction::PromoteToMemory,
        );
        let queue = ReviewQueue::open_project(root).unwrap();
        queue
            .enqueue_candidates(&LearningSessionId::new("seed"), &[candidate])
            .unwrap();
        let item = crate::ops::review_list(root).unwrap().remove(0);
        crate::ops::review_decide(root, &item.id, ReviewVerdict::Accept, "tester", None).unwrap();
        crate::ops::promote(root, &item.id).unwrap();
    }

    #[tokio::test]
    async fn returns_an_accepted_memory_that_matches() {
        let dir = tempfile::tempdir().unwrap();
        seed_accepted_memory(
            dir.path(),
            "always run the integration suite for exporter changes",
        );
        let ws = Workspace::new(dir.path()).unwrap();

        let out = MemorySearch
            .invoke(json!({ "query": "exporter" }), &context(&ws))
            .await
            .unwrap();

        assert!(!out.is_error);
        assert!(
            out.text.contains("Accepted memory matches"),
            "got: {}",
            out.text
        );
        assert!(out.text.contains("integration suite"), "got: {}", out.text);
    }

    #[tokio::test]
    async fn an_empty_store_returns_a_useful_message_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        crate::initialize(dir.path()).unwrap();
        let ws = Workspace::new(dir.path()).unwrap();

        let out = MemorySearch
            .invoke(json!({ "query": "anything" }), &context(&ws))
            .await
            .unwrap();

        assert!(!out.is_error, "an empty store must not be an error");
        assert!(out.text.contains("no accepted memory"), "got: {}", out.text);
    }

    #[tokio::test]
    async fn a_bare_prompt_creates_no_project_files() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();

        let out = MemorySearch
            .invoke(json!({ "query": "anything" }), &context(&ws))
            .await
            .unwrap();

        assert!(!out.is_error);
        assert!(out.text.contains("no accepted memory"));
        assert!(!dir.path().join(".localmind.toml").exists());
        assert!(!dir.path().join(".localmind").exists());
    }

    #[tokio::test]
    async fn an_empty_query_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let result = MemorySearch
            .invoke(json!({ "query": "   " }), &context(&ws))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidInput(_))));
    }

    #[test]
    fn the_effect_is_a_read_inside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let effects = MemorySearch
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
