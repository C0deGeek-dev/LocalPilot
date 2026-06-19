//! A model-callable tool that surfaces LocalMind's **active** (enabled) skills.
//!
//! Active skills are reviewable advisory prompt modules: a human enabled them
//! from a draft, and they carry provenance back to the accepted memory they were
//! distilled from. This tool surfaces them **read-only** so the agent can pull a
//! relevant skill into its reasoning as guidance — it is the explicit
//! consumption path for active skills. It never installs, enables, disables, or
//! executes a skill; lifecycle changes stay human, review-gated steps
//! (`localpilot learning skills`). This is the no-silent-execution contract for
//! skills (see `docs/localmind-integration.md`).

use async_trait::async_trait;
use localpilot_sandbox::Effect;
use localpilot_tools::{Tool, ToolContext, ToolError, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::fmt::Write as _;
use std::path::Path;

/// Bound on a single skill body, keeping injected guidance lean and budgeted.
const BODY_CHARS: usize = 2_000;

#[derive(Debug, Deserialize, JsonSchema)]
struct ActiveSkillsInput {
    /// When set, show this active skill's full `SKILL.md` body; otherwise list
    /// all active skills.
    #[serde(default)]
    skill_id: Option<String>,
}

/// Lists active (enabled) skills, or shows one skill's body. Read-only; never
/// enables, disables, installs, or runs a skill.
pub struct ActiveSkills;

#[async_trait]
impl Tool for ActiveSkills {
    fn name(&self) -> &str {
        "active_skills"
    }

    fn description(&self) -> &str {
        "List LocalMind's active (human-enabled) skills for this project, or show one skill's body \
         by id. Active skills are reviewable advisory prompt modules distilled from accepted \
         project memory, carrying provenance to their source. Read-only guidance: reading a skill \
         does not run, install, enable, or disable anything — apply its guidance yourself and let \
         the user manage the skill lifecycle with `localpilot learning skills`."
    }

    fn schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(ActiveSkillsInput)).unwrap_or(Value::Null)
    }

    fn approval_detail(&self, input: &Value) -> String {
        input
            .get("skill_id")
            .and_then(Value::as_str)
            .unwrap_or("(list all)")
            .chars()
            .take(160)
            .collect()
    }

    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        // Only reads the derived skill store under the project root.
        Ok(vec![Effect::ReadPath {
            inside_workspace: true,
            secret_like: false,
        }])
    }

    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: ActiveSkillsInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let root = ctx.workspace.root();

        match input
            .skill_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            Some(id) => Ok(show_skill(root, id)),
            None => Ok(list_skills(root)),
        }
    }
}

fn list_skills(root: &Path) -> ToolOutput {
    let skills = match crate::ops::skills_active_readonly(root) {
        Ok(skills) => skills,
        Err(_) => {
            return ToolOutput::ok(
                "active-skill store is unreadable; a human manages skills with \
                 `localpilot learning skills`",
            )
        }
    };
    if skills.is_empty() {
        return ToolOutput::ok(
            "no active skills (a human enables them from drafts via \
             `localpilot learning skills`)",
        );
    }
    let mut out = String::from("Active skills (advisory guidance — not executed automatically):\n");
    for skill in &skills {
        let _ = writeln!(out, "- [active] {} ({})", skill.name, skill.id);
    }
    out.push_str(
        "\nCall this tool with a skill id to read its body, then apply the guidance yourself. \
         Enabling/disabling skills stays a human step.",
    );
    ToolOutput::ok(out)
}

fn show_skill(root: &Path, id: &str) -> ToolOutput {
    let skills = match crate::ops::skills_active_readonly(root) {
        Ok(skills) => skills,
        Err(_) => {
            return ToolOutput::ok(
                "active-skill store is unreadable; a human manages skills with \
                 `localpilot learning skills`",
            )
        }
    };
    match skills.into_iter().find(|skill| skill.id == id) {
        Some(skill) => {
            let body: String = skill.body_markdown.chars().take(BODY_CHARS).collect();
            ToolOutput::ok(format!(
                "Active skill {} (advisory — apply as guidance, do not execute)\nname: {}\n\n{}",
                skill.id, skill.name, body
            ))
        }
        None => ToolOutput::ok(format!("no active skill with id \"{id}\"")),
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
            processes: None,
        }
    }

    /// Seed one active skill: enqueue a candidate-skill lesson, accept it,
    /// generate a draft, then have a human enable (activate) it. Returns its id.
    fn seed_active_skill(root: &Path) -> String {
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
        let draft_id = drafts[0].id.clone();
        // The human activation step.
        let activated = crate::ops::skill_activate(root, &draft_id).unwrap();
        activated.expect("activation returns the active skill").id
    }

    #[tokio::test]
    async fn lists_active_skills_as_advisory() {
        let dir = tempfile::tempdir().unwrap();
        let id = seed_active_skill(dir.path());
        let ws = Workspace::new(dir.path()).unwrap();

        let out = ActiveSkills.invoke(json!({}), &context(&ws)).await.unwrap();

        assert!(!out.is_error);
        assert!(
            out.text.contains(&id),
            "the skill id must be listed: {}",
            out.text
        );
        assert!(out.text.contains("[active]"), "got: {}", out.text);
        assert!(
            out.text.to_lowercase().contains("not executed"),
            "the advisory/no-execution framing must be explicit: {}",
            out.text
        );
    }

    #[tokio::test]
    async fn shows_a_skill_body_as_guidance_not_an_action() {
        let dir = tempfile::tempdir().unwrap();
        let id = seed_active_skill(dir.path());
        let ws = Workspace::new(dir.path()).unwrap();

        let out = ActiveSkills
            .invoke(json!({ "skill_id": id }), &context(&ws))
            .await
            .unwrap();

        assert!(!out.is_error);
        assert!(
            out.text.to_lowercase().contains("do not execute"),
            "the body must be framed as advisory guidance: {}",
            out.text
        );
    }

    #[tokio::test]
    async fn no_active_skills_is_graceful_and_creates_no_project_files() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();

        let out = ActiveSkills.invoke(json!({}), &context(&ws)).await.unwrap();

        assert!(!out.is_error);
        assert!(out.text.contains("no active skills"), "got: {}", out.text);
        // Read-only: a bare prompt never initializes the project.
        assert!(!dir.path().join(".localmind.toml").exists());
        assert!(!dir.path().join(".localmind").exists());
    }

    #[test]
    fn the_effect_is_a_read_inside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let effects = ActiveSkills.effects(&json!({}), &context(&ws)).unwrap();
        assert_eq!(
            effects,
            vec![Effect::ReadPath {
                inside_workspace: true,
                secret_like: false
            }]
        );
    }
}
