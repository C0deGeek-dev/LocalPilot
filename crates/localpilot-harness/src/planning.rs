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
gaps.";

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
    async fn exhausted_retries_returns_a_provider_error() {
        let provider = FakeProvider::new()
            .text("not a brief")
            .text("still not")
            .text("nope");
        let err = run_intake(&provider, "m", "idea").await.unwrap_err();
        assert!(matches!(err, HarnessError::Provider(_)));
    }
}
