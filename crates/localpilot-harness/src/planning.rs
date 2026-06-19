//! Intake and planning: turn an idea into a `brief.md`, and a brief into a
//! `PROGRESS.md`, using original LocalPilot prompts.
//!
//! Generated documents are validated before they are returned; invalid model
//! output is retried with the parse error fed back, up to a small cap.

use futures::StreamExt;
use localpilot_core::{Message, Role};
use localpilot_llm::{ModelEvent, ModelProvider, ModelRequest};

use crate::brief::Brief;
use crate::error::HarnessError;
use crate::progress::Progress;

/// The original LocalPilot intake prompt.
pub const INTAKE_PROMPT: &str = "\
You are the intake assistant for a software project. Turn the user's rough idea \
into a precise project brief.\n\
\n\
Respond with ONLY a Markdown document in exactly this shape, with these headings, \
and nothing else:\n\
\n\
# Brief: <short name>\n\
\n\
## Summary\n\
<one short paragraph>\n\
\n\
## Requirements\n\
- <requirement>\n\
\n\
## Constraints\n\
- <constraint>\n\
\n\
## Non-Goals\n\
- <thing explicitly out of scope>\n\
\n\
## Acceptance Criteria\n\
- <observable, testable criterion>\n\
\n\
## Risks & Rollback\n\
- <what could go wrong once this ships, and how it is undone: revert, feature \
flag, config switch, or migration down>\n\
\n\
Be concrete and testable. Prefer fewer, sharper items over many vague ones.";

/// The original LocalPilot planner prompt.
pub const PLANNER_PROMPT: &str = "\
You are the planning assistant for a software project. Given a project brief and a \
short repository summary, produce an ordered, test-first implementation plan.\n\
\n\
Respond with ONLY a Markdown document in exactly this shape, and nothing else:\n\
\n\
# Progress: <short name>\n\
Branch: feature/<kebab-name>\n\
\n\
## Steps\n\
\n\
- [ ] 1. <small, verifiable step>\n\
- [ ] 2. <next step>\n\
\n\
Each step must be small enough to complete and verify in one sitting, ordered so \
that tests come before the implementation they cover. Number steps from 1 with no \
gaps.\n\
\n\
Study the repository summary before writing steps. Where existing code already \
covers part of the work, prefer a step that extends or reuses it, naming that \
module, type, or function in the step, over adding parallel code; add new code \
only where nothing existing fits.\n\
\n\
The steps together must satisfy every acceptance criterion in the brief; do not \
leave a criterion unaddressed.";

const MAX_ATTEMPTS: usize = 3;

/// Generate a validated [`Brief`] from a rough idea.
///
/// # Errors
/// Returns [`HarnessError::Provider`] if the provider fails or never produces a
/// valid brief within the retry cap.
pub async fn run_intake(
    provider: &dyn ModelProvider,
    model: &str,
    idea: &str,
) -> Result<Brief, HarnessError> {
    let seed = vec![
        Message::text(Role::System, INTAKE_PROMPT),
        Message::text(Role::User, idea),
    ];
    generate(provider, model, seed, "brief.md", Brief::parse).await
}

/// Generate a validated [`Progress`] plan from a brief and a repo summary.
///
/// # Errors
/// Returns [`HarnessError::Provider`] if the provider fails or never produces a
/// valid plan within the retry cap.
pub async fn run_plan(
    provider: &dyn ModelProvider,
    model: &str,
    brief: &Brief,
    repo_summary: &str,
) -> Result<Progress, HarnessError> {
    let user = format!(
        "Project brief:\n\n{}\n\nRepository summary:\n\n{repo_summary}",
        brief.render()
    );
    let seed = vec![
        Message::text(Role::System, PLANNER_PROMPT),
        Message::text(Role::User, user),
    ];
    generate(provider, model, seed, "PROGRESS.md", Progress::parse).await
}

async fn generate<T>(
    provider: &dyn ModelProvider,
    model: &str,
    mut messages: Vec<Message>,
    document: &'static str,
    parse: impl Fn(&str) -> Result<T, HarnessError>,
) -> Result<T, HarnessError> {
    let mut last_error = String::new();
    for _ in 0..MAX_ATTEMPTS {
        let text = complete_text(provider, model, messages.clone()).await?;
        match parse(&text) {
            Ok(value) => return Ok(value),
            Err(err) => {
                last_error = err.to_string();
                messages.push(Message::text(Role::Assistant, text));
                messages.push(Message::text(
                    Role::User,
                    format!(
                        "That document was not valid: {last_error}. Reply again with ONLY the \
                         corrected Markdown in the required shape."
                    ),
                ));
            }
        }
    }
    Err(HarnessError::Provider(format!(
        "model did not produce a valid {document} after {MAX_ATTEMPTS} attempts: {last_error}"
    )))
}

async fn complete_text(
    provider: &dyn ModelProvider,
    model: &str,
    messages: Vec<Message>,
) -> Result<String, HarnessError> {
    let request = ModelRequest::new(model, messages);
    let mut stream = provider
        .stream(request)
        .await
        .map_err(|e| HarnessError::Provider(e.to_string()))?;
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event.map_err(|e| HarnessError::Provider(e.to_string()))? {
            ModelEvent::TextDelta(delta) => text.push_str(&delta),
            ModelEvent::Done => break,
            _ => {}
        }
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_llm::FakeProvider;

    const VALID_BRIEF: &str = "# Brief: thing\n\n## Summary\n\nDo the thing.\n\n\
## Requirements\n\n- It works\n\n## Constraints\n\n- Be small\n\n\
## Non-Goals\n\n- World peace\n\n## Acceptance Criteria\n\n- A test passes\n";

    #[test]
    fn intake_prompt_is_stable() {
        insta::assert_snapshot!(INTAKE_PROMPT);
    }

    #[test]
    fn planner_prompt_is_stable() {
        insta::assert_snapshot!(PLANNER_PROMPT);
    }

    #[tokio::test]
    async fn intake_produces_a_brief_from_an_idea() {
        let provider = FakeProvider::new().text(VALID_BRIEF);
        let brief = run_intake(&provider, "m", "build a thing").await.unwrap();
        assert_eq!(brief.name, "thing");
        assert_eq!(brief.requirements, vec!["It works"]);
    }

    #[tokio::test]
    async fn invalid_output_is_retried_with_feedback() {
        // First response is malformed (missing sections), second is valid.
        let provider = FakeProvider::new()
            .text("# Brief: thing\n\n## Summary\n\nincomplete\n")
            .text(VALID_BRIEF);
        let brief = run_intake(&provider, "m", "build a thing").await.unwrap();
        assert_eq!(brief.name, "thing");
    }

    #[tokio::test]
    async fn planner_is_asked_to_reuse_existing_code_and_cover_every_criterion() {
        use localpilot_core::ContentBlock;

        let brief = Brief::parse(VALID_BRIEF).unwrap();
        let valid_progress =
            "# Progress: thing\nBranch: feature/thing\n\n## Steps\n\n- [ ] 1. Do the thing\n";
        let provider = FakeProvider::new().text(valid_progress);
        let repo_summary =
            "Existing modules: localpilot_store::SessionStore already handles persistence.";

        run_plan(&provider, "m", &brief, repo_summary)
            .await
            .unwrap();

        let reqs = provider.requests();
        let messages = &reqs.first().expect("a planner request").messages;
        let role_text = |system: bool| {
            messages
                .iter()
                .filter(|m| {
                    if system {
                        matches!(m.role, Role::System)
                    } else {
                        matches!(m.role, Role::User)
                    }
                })
                .flat_map(|m| &m.content)
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let system = role_text(true);
        let user = role_text(false);

        // The planner is instructed to reuse existing code and to cover every criterion.
        assert!(
            system.contains("extends or reuses"),
            "reuse-before-add instruction missing from planner prompt"
        );
        assert!(
            system.contains("every acceptance criterion"),
            "criteria-coverage instruction missing from planner prompt"
        );
        // ...and it is handed the repository summary and the brief's acceptance criteria
        // so it can honour both.
        assert!(
            user.contains("SessionStore"),
            "repository summary not passed to the planner"
        );
        assert!(
            user.contains("A test passes"),
            "brief acceptance criteria not passed to the planner"
        );
    }

    #[tokio::test]
    async fn exhausted_retries_returns_a_provider_error() {
        let provider = FakeProvider::new()
            .text("not a brief")
            .text("still not")
            .text("nope");
        let err = run_intake(&provider, "m", "idea").await.unwrap_err();
        assert!(matches!(err, HarnessError::Provider(_)));
    }
}
