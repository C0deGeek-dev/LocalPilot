//! A model-callable tool that surfaces LocalMind's generated skill *drafts*.
//!
//! Drafts are candidate, reusable workflows distilled from accepted project
//! memory. They are always created disabled and are never auto-installed or
//! auto-activated: this tool only *surfaces* them read-only so the agent can
//! notice a relevant draft and propose it to the user. Enabling a draft stays a
//! human, review-gated step (ADR-0011/0013) — there is no activation path here.

use async_trait::async_trait;
use localpilot_sandbox::Effect;
use localpilot_tools::{Tool, ToolContext, ToolError, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::fmt::Write as _;
use std::path::Path;

/// Bound on the rendered body of a single draft, keeping the result lean.
const BODY_CHARS: usize = 2_000;

#[derive(Debug, Deserialize, JsonSchema)]
struct SkillDraftsInput {
    /// When set, show this draft's full `SKILL.md` body; otherwise list all
    /// drafts.
    #[serde(default)]
    draft_id: Option<String>,
}

/// Lists generated skill drafts (or shows one draft's body). Read-only; never
/// activates a draft.
pub struct SkillDrafts;

#[async_trait]
impl Tool for SkillDrafts {
    fn name(&self) -> &str {
        "skill_drafts"
    }

    fn description(&self) -> &str {
        "List LocalMind's generated skill drafts for this project, or show one draft's body by id. \
         Drafts are candidate reusable workflows distilled from accepted project memory; they are \
         always disabled and never auto-installed. Read-only: surfacing a draft does not enable it \
         — enabling stays a human step (`localpilot learning skills`). Use it to notice a relevant \
         existing workflow and propose it to the user, not to apply one yourself."
    }

    fn schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(SkillDraftsInput)).unwrap_or(Value::Null)
    }

    fn approval_detail(&self, input: &Value) -> String {
        input
            .get("draft_id")
            .and_then(Value::as_str)
            .unwrap_or("(list all)")
            .chars()
            .take(160)
            .collect()
    }

    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        // Only reads the derived skill-draft store under the project root.
        Ok(vec![Effect::ReadPath {
            inside_workspace: true,
            secret_like: false,
        }])
    }

    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: SkillDraftsInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let root = ctx.workspace.root();

        match input
            .draft_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            Some(id) => Ok(show_draft(root, id)),
            None => Ok(list_drafts(root)),
        }
    }
}

/// The status label for a draft. Drafts are disabled by construction; reporting
/// it explicitly keeps the agent from mistaking a draft for an active skill.
fn status_label(disabled: bool) -> &'static str {
    if disabled {
        "disabled"
    } else {
        "active"
    }
}

fn list_drafts(root: &Path) -> ToolOutput {
    let drafts = match crate::ops::skill_drafts_readonly(root) {
        Ok(drafts) => drafts,
        Err(_) => {
            return ToolOutput::ok(
                "skill-draft store is unreadable; regenerate it with \
                 `localpilot learning skills generate`",
            )
        }
    };
    if drafts.is_empty() {
        return ToolOutput::ok(
            "no skill drafts yet (they are generated from accepted project memory via \
             `localpilot learning skills generate`)",
        );
    }
    let mut out = String::from(
        "Generated skill drafts (suggestions only — disabled until a human enables them):\n",
    );
    for draft in &drafts {
        let status = status_label(draft.disabled);
        let _ = writeln!(
            out,
            "- [{status}] {} ({}) — {}",
            draft.name, draft.id, draft.description
        );
    }
    out.push_str(
        "\nThese are not active skills. To inspect one, call this tool with its id; to enable one, \
         a human runs `localpilot learning skills`.",
    );
    ToolOutput::ok(out)
}

fn show_draft(root: &Path, id: &str) -> ToolOutput {
    match crate::ops::skill_draft_detail_readonly(root, id) {
        Ok(Some((info, body))) => {
            let status = status_label(info.disabled);
            let body: String = body.chars().take(BODY_CHARS).collect();
            ToolOutput::ok(format!(
                "Skill draft {} ({status} — a suggestion, not an active skill)\nname: {}\n\
                 description: {}\npath: {}\n\n{}",
                info.id, info.name, info.description, info.path, body
            ))
        }
        Ok(None) => ToolOutput::ok(format!("no skill draft with id \"{id}\"")),
        Err(_) => ToolOutput::ok(
            "skill-draft store is unreadable; regenerate it with \
             `localpilot learning skills generate`",
        ),
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

    fn context(workspace: &Workspace) -> ToolContext<'_> {
        ToolContext {
            workspace,
            interactivity: Interactivity::NonInteractive,
            trusted: true,
            retention: None,
        }
    }

    /// Seed one generated, disabled skill draft and return its id: enqueue a
    /// candidate-skill lesson, accept it, then generate drafts from the queue.
    fn seed_draft(root: &Path) -> String {
        crate::initialize(root).unwrap();
        let candidate = CandidateLesson::new(
            LessonId::new("seed-skill"),
            "Run the exporter integration suite after touching the writer.".to_string(),
            LessonCategory::CandidateSkill,
            Confidence::new(0.9).unwrap(),
            SuggestedAction::CreateSkillDraft,
        );
        let queue = ReviewQueue::open_project(root).unwrap();
        queue
            .enqueue_candidates(&LearningSessionId::new("seed"), &[candidate])
            .unwrap();
        let item = crate::ops::review_list(root).unwrap().remove(0);
        crate::ops::review_decide(root, &item.id, ReviewVerdict::Accept, "tester", None).unwrap();
        let drafts = crate::ops::skills_generate(root).unwrap();
        assert_eq!(drafts.len(), 1, "expected one generated draft");
        assert!(drafts[0].disabled, "generated drafts must be disabled");
        drafts[0].id.clone()
    }

    #[tokio::test]
    async fn lists_generated_drafts_as_disabled_suggestions() {
        let dir = tempfile::tempdir().unwrap();
        let id = seed_draft(dir.path());
        let ws = Workspace::new(dir.path()).unwrap();

        let out = SkillDrafts.invoke(json!({}), &context(&ws)).await.unwrap();

        assert!(!out.is_error);
        assert!(
            out.text.contains(&id),
            "the draft id must be listed: {}",
            out.text
        );
        assert!(out.text.contains("[disabled]"), "got: {}", out.text);
        // A draft is never surfaced as an active skill.
        assert!(
            !out.text.contains("[active]"),
            "a draft must not read as active: {}",
            out.text
        );
    }

    #[tokio::test]
    async fn shows_a_single_draft_body_marked_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let id = seed_draft(dir.path());
        let ws = Workspace::new(dir.path()).unwrap();

        let out = SkillDrafts
            .invoke(json!({ "draft_id": id }), &context(&ws))
            .await
            .unwrap();

        assert!(!out.is_error);
        assert!(
            out.text.contains("disabled"),
            "the body must report disabled: {}",
            out.text
        );
        assert!(
            !out.text.contains("(active"),
            "a draft must not be shown as active: {}",
            out.text
        );
    }

    #[tokio::test]
    async fn an_unknown_draft_id_is_a_useful_result_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        seed_draft(dir.path());
        let ws = Workspace::new(dir.path()).unwrap();

        let out = SkillDrafts
            .invoke(json!({ "draft_id": "skill-does-not-exist" }), &context(&ws))
            .await
            .unwrap();

        assert!(!out.is_error);
        assert!(
            out.text.contains("no skill draft with id"),
            "got: {}",
            out.text
        );
    }

    #[tokio::test]
    async fn no_drafts_is_graceful_and_creates_no_project_files() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();

        let out = SkillDrafts.invoke(json!({}), &context(&ws)).await.unwrap();

        assert!(!out.is_error, "an absent store must not be an error");
        assert!(out.text.contains("no skill drafts"), "got: {}", out.text);
        // Read-only: a bare prompt never initializes the project.
        assert!(!dir.path().join(".localmind.toml").exists());
        assert!(!dir.path().join(".localmind").exists());
    }

    #[test]
    fn the_effect_is_a_read_inside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let effects = SkillDrafts.effects(&json!({}), &context(&ws)).unwrap();
        assert_eq!(
            effects,
            vec![Effect::ReadPath {
                inside_workspace: true,
                secret_like: false
            }]
        );
    }
}
