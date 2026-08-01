//! `ask_user` — put a decision to the user instead of guessing.
//!
//! The model has no other way to ask. Without this it can only write the
//! question into its answer as prose, which the user has to find, interpret, and
//! reply to by hand — so in practice ambiguity gets resolved by a silent guess.
//!
//! The tool declares **no effects**, so the permission engine never gates it.
//! The real gate is the host capability: a profile that grants everything still
//! cannot conjure a user, and a profile that grants nothing does not stop one
//! being asked. Where there is no human — a piped run, a CI run, a subagent —
//! the tool says so and the model proceeds on its own judgment; it never waits.

use async_trait::async_trait;
use localpilot_sandbox::{Effect, Interactivity};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::error::ToolError;
use crate::tool::{
    parse_input, schema_for, QuestionOption, Tool, ToolContext, ToolOutput, UserAnswer,
    UserQuestion,
};

/// The model-callable name.
pub const ASK_USER: &str = "ask_user";

/// Bounds on one call. A question the user cannot answer in a glance is not a
/// question, it is a form.
const MIN_QUESTIONS: usize = 1;
const MAX_QUESTIONS: usize = 4;
const MIN_OPTIONS: usize = 2;
const MAX_OPTIONS: usize = 4;

/// What the model says when no human is reachable. Mirrors `delegate`'s
/// unavailable path: a model-visible string, not an error and not a wait.
const UNAVAILABLE: &str = "ask_user is not available in this session (non-interactive). Pick the \
                           most reasonable option and state the assumption in your answer.";

#[derive(Debug, Deserialize, JsonSchema)]
struct AskUserInput {
    /// The questions to ask, in order. One to four.
    questions: Vec<QuestionInput>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct QuestionInput {
    /// A short label for the question's topic (a few words).
    #[serde(default)]
    header: Option<String>,
    /// The question to put to the user.
    question: String,
    /// Two to four distinct answers to offer. The user can always type
    /// something else instead, so these are a best guess, not a ceiling.
    options: Vec<OptionInput>,
    /// Whether several options may be chosen together. Only for choices that
    /// genuinely combine.
    #[serde(default)]
    multi_select: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct OptionInput {
    /// The answer text the user picks.
    label: String,
    /// What choosing this means, when the label alone is not enough.
    #[serde(default)]
    description: Option<String>,
}

/// Ask the user one to four multiple-choice questions.
pub struct AskUser;

#[async_trait]
impl Tool for AskUser {
    fn name(&self) -> &str {
        ASK_USER
    }

    fn description(&self) -> &str {
        "Ask the user to decide between concrete options when different readings of the request \
         would lead to materially different work, or before something hard to undo. Each question \
         offers 2-4 options; the user can also answer freely. Do not use it for choices with an \
         obvious default, for permission to do work already asked for, or to report progress."
    }

    fn schema(&self) -> Value {
        schema_for::<AskUserInput>()
    }

    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        // Asking a question touches nothing. The host capability is the gate.
        Ok(Vec::new())
    }

    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: AskUserInput = parse_input(&input)?;
        let questions = validate(input)?;

        // No human on this session: answer the model rather than stalling it.
        // Checked after validation so a malformed call is still reported as
        // malformed — a headless run is where that mistake would otherwise hide.
        let Some(prompter) = ctx.prompter else {
            return Ok(ToolOutput::ok(UNAVAILABLE));
        };
        if ctx.interactivity == Interactivity::NonInteractive {
            return Ok(ToolOutput::ok(UNAVAILABLE));
        }

        let answers = prompter.ask(&questions).await;
        Ok(ToolOutput::ok(render_transcript(&questions, &answers)))
    }
}

/// Check a call before a human is made to look at it: the wrong number of
/// questions or options, an empty question, or duplicate labels within one
/// question are all the model's mistake to fix, not the user's to puzzle over.
fn validate(input: AskUserInput) -> Result<Vec<UserQuestion>, ToolError> {
    let count = input.questions.len();
    if !(MIN_QUESTIONS..=MAX_QUESTIONS).contains(&count) {
        return Err(ToolError::InvalidInput(format!(
            "ask between {MIN_QUESTIONS} and {MAX_QUESTIONS} questions, got {count}"
        )));
    }
    let mut questions = Vec::with_capacity(count);
    for (index, question) in input.questions.into_iter().enumerate() {
        let position = index + 1;
        if question.question.trim().is_empty() {
            return Err(ToolError::InvalidInput(format!(
                "question {position} is empty"
            )));
        }
        let option_count = question.options.len();
        if !(MIN_OPTIONS..=MAX_OPTIONS).contains(&option_count) {
            return Err(ToolError::InvalidInput(format!(
                "question {position} must offer between {MIN_OPTIONS} and {MAX_OPTIONS} options, \
                 got {option_count}"
            )));
        }
        let mut labels: Vec<String> = Vec::with_capacity(option_count);
        for option in &question.options {
            let label = option.label.trim();
            if label.is_empty() {
                return Err(ToolError::InvalidInput(format!(
                    "question {position} has an option with an empty label"
                )));
            }
            if labels.iter().any(|seen| seen.eq_ignore_ascii_case(label)) {
                return Err(ToolError::InvalidInput(format!(
                    "question {position} repeats the option {label:?}"
                )));
            }
            labels.push(label.to_string());
        }
        questions.push(UserQuestion {
            header: question
                .header
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty()),
            question: question.question.trim().to_string(),
            options: question
                .options
                .into_iter()
                .map(|option| QuestionOption {
                    label: option.label.trim().to_string(),
                    description: option
                        .description
                        .map(|d| d.trim().to_string())
                        .filter(|d| !d.is_empty()),
                })
                .collect(),
            multi_select: question.multi_select,
        });
    }
    Ok(questions)
}

/// Pair each question with what the user answered, as the model reads it.
fn render_transcript(questions: &[UserQuestion], answers: &[UserAnswer]) -> String {
    let mut out = String::new();
    for (index, question) in questions.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&format!("Q: {}\n", question.question));
        let answer = match answers.get(index) {
            Some(UserAnswer::Selected(labels)) if !labels.is_empty() => labels.join(", "),
            // A dismissed question is not a failure; it hands the decision back.
            Some(UserAnswer::Other(text)) if !text.trim().is_empty() => text.trim().to_string(),
            _ => "(no answer — use your own judgment and say which way you went)".to_string(),
        };
        out.push_str(&format!("A: {answer}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use localpilot_sandbox::Workspace;
    use serde_json::json;

    /// A prompter that answers with a fixed script.
    struct ScriptedPrompter(Vec<UserAnswer>);

    impl crate::tool::UserPrompter for ScriptedPrompter {
        fn ask<'a>(
            &'a self,
            _questions: &'a [UserQuestion],
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<UserAnswer>> + Send + 'a>>
        {
            Box::pin(async move { self.0.clone() })
        }
    }

    fn one_question() -> Value {
        json!({
            "questions": [{
                "header": "Storage",
                "question": "Which store should this use?",
                "options": [
                    { "label": "SQLite", "description": "one file, no server" },
                    { "label": "Postgres" }
                ]
            }]
        })
    }

    async fn invoke_with(
        input: Value,
        prompter: Option<&dyn crate::tool::UserPrompter>,
        interactivity: Interactivity,
    ) -> Result<ToolOutput, ToolError> {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(dir.path()).unwrap();
        let ctx = ToolContext {
            workspace: &workspace,
            interactivity,
            trusted: true,
            retention: None,
            processes: None,
            agents: None,
            prompter,
        };
        AskUser.invoke(input, &ctx).await
    }

    #[tokio::test]
    async fn a_well_formed_call_pairs_each_question_with_its_answer() {
        let prompter = ScriptedPrompter(vec![UserAnswer::Selected(vec!["SQLite".to_string()])]);
        let out = invoke_with(one_question(), Some(&prompter), Interactivity::Interactive)
            .await
            .unwrap();
        assert!(out.text.contains("Q: Which store should this use?"));
        assert!(out.text.contains("A: SQLite"));
    }

    #[tokio::test]
    async fn free_text_comes_back_as_the_answer() {
        let prompter = ScriptedPrompter(vec![UserAnswer::Other("DuckDB, actually".to_string())]);
        let out = invoke_with(one_question(), Some(&prompter), Interactivity::Interactive)
            .await
            .unwrap();
        assert!(out.text.contains("A: DuckDB, actually"));
    }

    #[tokio::test]
    async fn a_dismissed_question_hands_the_decision_back() {
        let prompter = ScriptedPrompter(vec![UserAnswer::Dismissed]);
        let out = invoke_with(one_question(), Some(&prompter), Interactivity::Interactive)
            .await
            .unwrap();
        assert!(
            out.text.contains("use your own judgment"),
            "a dismissal is guidance, not an error: {}",
            out.text
        );
    }

    #[tokio::test]
    async fn malformed_calls_are_rejected_before_a_human_sees_them() {
        let cases = vec![
            (json!({ "questions": [] }), "zero questions"),
            (
                json!({ "questions": (0..5).map(|i| json!({
                    "question": format!("q{i}"),
                    "options": [{ "label": "a" }, { "label": "b" }]
                })).collect::<Vec<_>>() }),
                "five questions",
            ),
            (
                json!({ "questions": [{ "question": "q", "options": [{ "label": "only" }] }] }),
                "one option",
            ),
            (
                json!({ "questions": [{ "question": "q", "options": (0..5)
                    .map(|i| json!({ "label": format!("o{i}") })).collect::<Vec<_>>() }] }),
                "five options",
            ),
            (
                json!({ "questions": [{ "question": "  ", "options": [
                    { "label": "a" }, { "label": "b" }
                ] }] }),
                "a blank question",
            ),
            (
                json!({ "questions": [{ "question": "q", "options": [
                    { "label": "Same" }, { "label": "same" }
                ] }] }),
                "duplicate labels",
            ),
        ];
        let prompter = ScriptedPrompter(vec![UserAnswer::Dismissed]);
        for (input, case) in cases {
            let result = invoke_with(input, Some(&prompter), Interactivity::Interactive).await;
            assert!(
                matches!(result, Err(ToolError::InvalidInput(_))),
                "{case} must be rejected, got {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn no_prompter_reports_unavailable_rather_than_waiting() {
        let out = invoke_with(one_question(), None, Interactivity::Interactive)
            .await
            .unwrap();
        assert_eq!(out.text, UNAVAILABLE);
        assert_eq!(out.outcome, localpilot_core::ToolOutcome::Ok);
    }

    #[tokio::test]
    async fn a_non_interactive_session_reports_unavailable_even_with_a_prompter() {
        let prompter = ScriptedPrompter(vec![UserAnswer::Selected(vec!["SQLite".to_string()])]);
        let out = invoke_with(
            one_question(),
            Some(&prompter),
            Interactivity::NonInteractive,
        )
        .await
        .unwrap();
        assert_eq!(out.text, UNAVAILABLE);
    }

    #[tokio::test]
    async fn multi_select_answers_are_joined() {
        let prompter = ScriptedPrompter(vec![UserAnswer::Selected(vec![
            "SQLite".to_string(),
            "Postgres".to_string(),
        ])]);
        let out = invoke_with(one_question(), Some(&prompter), Interactivity::Interactive)
            .await
            .unwrap();
        assert!(out.text.contains("A: SQLite, Postgres"));
    }
}
