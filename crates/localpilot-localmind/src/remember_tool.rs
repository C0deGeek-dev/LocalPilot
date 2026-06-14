//! A model-callable tool that proposes a durable project lesson for review.
//!
//! This is the agent's in-session entry to the learning loop: when the agent
//! notices something worth keeping (a convention, a pitfall, a decision), it can
//! enqueue a *review candidate* — it never writes accepted memory directly.
//! Promotion stays a human, review-gated step (ADR-0011/0013).

use async_trait::async_trait;
use localpilot_sandbox::Effect;
use localpilot_tools::{Tool, ToolContext, ToolError, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::hash::{Hash as _, Hasher as _};

use localmind_core::{
    CandidateLesson, Confidence, EvidenceKind, EvidenceRef, LessonCategory, LessonId,
    SessionId as LearningSessionId, SuggestedAction,
};
use localmind_store::ReviewQueue;
use localpilot_config::redact;

/// Confidence assigned to an agent-proposed candidate. Modest — it is a
/// suggestion awaiting human review, not an established fact.
const REMEMBER_CONFIDENCE: f32 = 0.6;

#[derive(Debug, Deserialize, JsonSchema)]
struct RememberInput {
    /// The durable lesson to propose, in one or two sentences.
    lesson: String,
    /// Optional category hint: `convention`, `tooling`, `documentation`, or
    /// `skill`. Defaults to a project convention.
    #[serde(default)]
    category: Option<String>,
}

/// Proposes a durable project lesson as a review candidate. Never writes accepted
/// memory.
pub struct Remember;

#[async_trait]
impl Tool for Remember {
    fn name(&self) -> &str {
        "remember"
    }

    fn description(&self) -> &str {
        "Propose a durable project lesson for human review (LocalMind). Enqueues a review \
         candidate — it never writes accepted memory directly; a human accepts or rejects it \
         later. Use sparingly, for genuinely durable conventions, pitfalls, or decisions worth \
         keeping — not transient notes."
    }

    fn schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(RememberInput)).unwrap_or(Value::Null)
    }

    fn approval_detail(&self, input: &Value) -> String {
        input
            .get("lesson")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .chars()
            .take(160)
            .collect()
    }

    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        // Enqueuing a candidate writes to the project-local LocalMind store.
        Ok(vec![Effect::WritePath {
            inside_workspace: true,
            overwrite: false,
        }])
    }

    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: RememberInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let lesson = input.lesson.trim();
        if lesson.is_empty() {
            return Err(ToolError::InvalidInput(
                "lesson must not be empty".to_string(),
            ));
        }
        let root = ctx.workspace.root();
        // Redact at the host boundary before anything reaches the store.
        let redacted = redact::redact(lesson);
        let confidence =
            Confidence::new(REMEMBER_CONFIDENCE).map_err(|e| ToolError::Failed(e.to_string()))?;
        let candidate = CandidateLesson::new(
            LessonId::new(candidate_id(&redacted)),
            redacted.clone(),
            category_for(input.category.as_deref()),
            confidence,
            SuggestedAction::PromoteToMemory,
        )
        .with_evidence(
            EvidenceRef::new(
                EvidenceKind::Other("agent_remember".to_string()),
                "proposed in-session by the agent".to_string(),
            )
            .redacted(),
        );
        let queue =
            ReviewQueue::open_project(root).map_err(|e| ToolError::Failed(e.to_string()))?;
        queue
            .enqueue_candidates(&LearningSessionId::new("agent-remember"), &[candidate])
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        Ok(ToolOutput::ok(
            "noted — enqueued a candidate lesson for review (it is not accepted memory until a \
             human approves it via `localpilot learning review`)",
        ))
    }
}

/// A stable-enough id for de-duping repeated identical proposals within a run.
fn candidate_id(text: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    format!("remember-{:x}", hasher.finish())
}

fn category_for(hint: Option<&str>) -> LessonCategory {
    match hint.map(str::to_ascii_lowercase).as_deref() {
        Some("tooling") => LessonCategory::ToolingNote,
        Some("documentation" | "doc") => LessonCategory::DocumentationUpdate,
        Some("skill") => LessonCategory::CandidateSkill,
        _ => LessonCategory::ProjectConvention,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
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
    async fn enqueues_a_review_candidate_never_accepted_memory() {
        let dir = tempfile::tempdir().unwrap();
        crate::initialize(dir.path()).unwrap();
        let ws = Workspace::new(dir.path()).unwrap();

        let out = Remember
            .invoke(
                json!({ "lesson": "Prefer deterministic fixtures in extractor tests." }),
                &context(&ws),
            )
            .await
            .unwrap();
        assert!(!out.is_error);

        // It is a review candidate, visible in the review queue...
        let review = crate::ops::review_list(dir.path()).unwrap();
        assert_eq!(review.len(), 1);
        // ...and it is NOT accepted memory (search over accepted memory is empty).
        assert!(crate::ops::search(dir.path(), "deterministic")
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn an_empty_lesson_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let result = Remember
            .invoke(json!({ "lesson": "   " }), &context(&ws))
            .await;
        assert!(matches!(result, Err(ToolError::InvalidInput(_))));
    }

    #[test]
    fn the_effect_is_an_in_workspace_write() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let effects = Remember
            .effects(&json!({ "lesson": "x" }), &context(&ws))
            .unwrap();
        assert_eq!(
            effects,
            vec![Effect::WritePath {
                inside_workspace: true,
                overwrite: false
            }]
        );
    }
}
