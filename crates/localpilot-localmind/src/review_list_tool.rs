//! A model-callable tool that lists the project's LocalMind review queue.
//!
//! The agent proposes lessons (`remember`) and closeout enqueues candidates, but
//! it had no way to *see* its own queue — it would reverse-engineer SQL against
//! the sqlite store. This tool surfaces the queue read-only: pending candidates,
//! their state, and a count summary. It never decides or promotes anything —
//! review stays a human, review-gated step (ADR-0011/0013).

use async_trait::async_trait;
use localpilot_sandbox::Effect;
use localpilot_tools::{Tool, ToolContext, ToolError, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::fmt::Write as _;
use std::path::Path;

use crate::ops::ReviewSummary;

/// Default number of items listed when the caller does not ask for a count.
const DEFAULT_LIMIT: usize = 20;
/// Ceiling on listed items, so a single call cannot flood the context.
const MAX_LIMIT: usize = 50;

#[derive(Debug, Deserialize, JsonSchema)]
struct ReviewListInput {
    /// Optional state filter: `pending`, `accepted`, `rejected`, `edited`, or
    /// `deferred`. Omit to list every state.
    #[serde(default)]
    state: Option<String>,
    /// Maximum number of items to return (default 20, capped at 50).
    #[serde(default)]
    limit: Option<usize>,
}

/// Lists the project's LocalMind review queue with a state-count summary.
/// Read-only; never decides or promotes a candidate.
pub struct ReviewList;

#[async_trait]
impl Tool for ReviewList {
    fn name(&self) -> &str {
        "localmind_review_list"
    }

    fn description(&self) -> &str {
        "List the project's LocalMind review queue: pending candidate lessons awaiting human \
         review, plus a count of each state (pending/accepted/rejected/edited/deferred). Optional \
         `state` filter and `limit`. Read-only — it never accepts, rejects, or promotes anything; \
         reviewing stays a human step (`localpilot learning review`). Use it to see what is already \
         queued before proposing a new lesson with `remember`."
    }

    fn schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(ReviewListInput)).unwrap_or(Value::Null)
    }

    fn approval_detail(&self, input: &Value) -> String {
        input
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("(all states)")
            .chars()
            .take(160)
            .collect()
    }

    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        // Only reads the project-local LocalMind review store.
        Ok(vec![Effect::ReadPath {
            inside_workspace: true,
            secret_like: false,
        }])
    }

    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: ReviewListInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let limit = input.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let root = ctx.workspace.root();

        let items = match crate::ops::review_list_readonly(root) {
            Ok(items) => items,
            Err(_) => {
                return Ok(ToolOutput::ok(
                    "review queue is unreadable; inspect it with `localpilot learning review`",
                ))
            }
        };
        Ok(ToolOutput::ok(render(
            &items,
            input.state.as_deref(),
            limit,
        )))
    }
}

/// Lowercased state filter normalized to the stored state label, or `None` for
/// an unrecognized filter (which then matches nothing, reported explicitly).
fn normalize_state(filter: &str) -> Option<&'static str> {
    match filter.trim().to_ascii_lowercase().as_str() {
        "pending" => Some("Pending"),
        "accepted" => Some("Accepted"),
        "rejected" => Some("Rejected"),
        "edited" => Some("Edited"),
        "merged" => Some("Merged"),
        "deferred" => Some("Deferred"),
        _ => None,
    }
}

fn render(items: &[ReviewSummary], state_filter: Option<&str>, limit: usize) -> String {
    if items.is_empty() {
        return "review queue is empty (no candidate lessons enqueued yet)".to_string();
    }

    // The state field is the Debug form of the core enum, e.g. "Pending".
    let count = |state: &str| items.iter().filter(|i| i.state == state).count();
    let mut out = format!(
        "Review queue — {} total: {} pending, {} accepted, {} rejected, {} edited, {} deferred.\n",
        items.len(),
        count("Pending"),
        count("Accepted"),
        count("Rejected"),
        count("Edited"),
        count("Deferred"),
    );

    let wanted = state_filter.map(str::trim).filter(|s| !s.is_empty());
    let normalized = wanted.map(normalize_state);
    if let Some(None) = normalized {
        let _ = writeln!(
            out,
            "(unknown state filter \"{}\" — showing nothing; valid: pending, accepted, rejected, \
             edited, merged, deferred)",
            wanted.unwrap_or_default()
        );
        return out;
    }
    let target = normalized.flatten();

    let mut shown = 0;
    for item in items {
        if let Some(state) = target {
            if item.state != state {
                continue;
            }
        }
        if shown >= limit {
            break;
        }
        let _ = writeln!(
            out,
            "- [{}] {} ({:.2}, {}) — {}",
            item.state, item.id, item.confidence, item.category, item.summary
        );
        shown += 1;
    }
    if shown == 0 {
        let label = target.unwrap_or("matching");
        let _ = writeln!(out, "(no {label} items)");
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use localmind_core::{
        CandidateLesson, Confidence, LessonCategory, LessonId, SessionId as LearningSessionId,
        SuggestedAction,
    };
    use localmind_store::ReviewQueue;
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

    fn enqueue(root: &Path, id: &str, summary: &str) {
        crate::initialize(root).unwrap();
        let candidate = CandidateLesson::new(
            LessonId::new(id),
            summary.to_string(),
            LessonCategory::ProjectConvention,
            Confidence::new(0.6).unwrap(),
            SuggestedAction::PromoteToMemory,
        );
        let queue = ReviewQueue::open_project(root).unwrap();
        queue
            .enqueue_candidates(&LearningSessionId::new("seed"), &[candidate])
            .unwrap();
    }

    #[tokio::test]
    async fn lists_enqueued_candidates_with_a_state_summary() {
        let dir = tempfile::tempdir().unwrap();
        enqueue(
            dir.path(),
            "lesson-a",
            "prefer guard clauses over deep nesting",
        );
        let ws = Workspace::new(dir.path()).unwrap();

        let out = ReviewList.invoke(json!({}), &context(&ws)).await.unwrap();

        assert!(!out.is_error);
        assert!(out.text.contains("1 pending"), "got: {}", out.text);
        assert!(out.text.contains("lesson-a"), "got: {}", out.text);
        assert!(
            out.text.contains("prefer guard clauses"),
            "got: {}",
            out.text
        );
    }

    #[tokio::test]
    async fn an_empty_queue_is_a_useful_result_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        crate::initialize(dir.path()).unwrap();
        let ws = Workspace::new(dir.path()).unwrap();

        let out = ReviewList.invoke(json!({}), &context(&ws)).await.unwrap();

        assert!(!out.is_error, "an empty queue must not be an error");
        assert!(
            out.text.contains("review queue is empty"),
            "got: {}",
            out.text
        );
    }

    #[tokio::test]
    async fn a_bare_prompt_creates_no_project_files() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();

        let out = ReviewList.invoke(json!({}), &context(&ws)).await.unwrap();

        assert!(!out.is_error);
        assert!(out.text.contains("review queue is empty"));
        // Read-only: a bare prompt never initializes the project.
        assert!(!dir.path().join(".localmind.toml").exists());
        assert!(!dir.path().join(".localmind").exists());
    }

    #[tokio::test]
    async fn the_state_filter_limits_the_listing() {
        let dir = tempfile::tempdir().unwrap();
        enqueue(dir.path(), "lesson-a", "first pending lesson");
        let ws = Workspace::new(dir.path()).unwrap();

        // Nothing is accepted yet, so an accepted filter lists no items but still
        // reports the summary — not an error.
        let out = ReviewList
            .invoke(json!({ "state": "accepted" }), &context(&ws))
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.text.contains("1 pending"), "got: {}", out.text);
        assert!(out.text.contains("no Accepted items"), "got: {}", out.text);
    }

    #[test]
    fn the_effect_is_a_read_inside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let effects = ReviewList.effects(&json!({}), &context(&ws)).unwrap();
        assert_eq!(
            effects,
            vec![Effect::ReadPath {
                inside_workspace: true,
                secret_like: false
            }]
        );
    }
}
