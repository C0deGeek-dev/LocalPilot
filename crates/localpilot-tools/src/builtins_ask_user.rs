//! Interactive user elicitation through the host that owns the terminal.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use localpilot_sandbox::Effect;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::contract::{Idempotency, Reversibility, SideEffectClass, ToolContract};
use crate::error::ToolError;
use crate::tool::{detail_preview, parse_input, schema_for, Tool, ToolContext, ToolOutput};

const MAX_QUESTION_BYTES: usize = 4 * 1024;
const MAX_OPTION_BYTES: usize = 1024;
const MAX_ANSWER_BYTES: usize = 4 * 1024;
const MAX_OPTIONS: usize = 8;

/// One bounded question the interactive host presents to the user.
#[derive(Clone, PartialEq, Eq)]
pub struct ElicitationRequest {
    pub question: String,
    pub options: Vec<String>,
}

impl std::fmt::Debug for ElicitationRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ElicitationRequest")
            .field(
                "question",
                &format_args!("<{} bytes redacted>", self.question.len()),
            )
            .field(
                "options",
                &format_args!("<{} redacted>", self.options.len()),
            )
            .finish()
    }
}

/// The user's resolution of an elicitation request.
#[derive(Clone, PartialEq, Eq)]
pub enum ElicitationOutcome {
    Answered(String),
    Cancelled,
}

impl std::fmt::Debug for ElicitationOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Answered(answer) => formatter
                .debug_tuple("Answered")
                .field(&format_args!("<{} bytes redacted>", answer.len()))
                .finish(),
            Self::Cancelled => formatter.write_str("Cancelled"),
        }
    }
}

/// Host bridge used by [`AskUser`]. A closed or unavailable host resolves as a
/// cancellation; the tool never guesses an answer.
pub trait UserElicitor: Send + Sync {
    fn ask(
        &self,
        request: ElicitationRequest,
    ) -> Pin<Box<dyn Future<Output = ElicitationOutcome> + Send + '_>>;
}

#[derive(Deserialize, JsonSchema)]
struct AskUserInput {
    /// A concise question for the user.
    question: String,
    /// Two or more mutually exclusive choices. The host adds a free-text Other
    /// option, so do not include one here.
    options: Vec<String>,
}

/// Ask the user one bounded multiple-choice question through the active host.
pub struct AskUser {
    elicitor: Arc<dyn UserElicitor>,
}

impl AskUser {
    #[must_use]
    pub fn new(elicitor: Arc<dyn UserElicitor>) -> Self {
        Self { elicitor }
    }

    fn request(input: &Value) -> Result<ElicitationRequest, ToolError> {
        let input: AskUserInput = parse_input(input)?;
        let question = input.question.trim();
        if question.is_empty() {
            return Err(ToolError::InvalidInput(
                "`question` must not be empty".to_string(),
            ));
        }
        if question.len() > MAX_QUESTION_BYTES {
            return Err(ToolError::InvalidInput(format!(
                "`question` exceeds the {MAX_QUESTION_BYTES}-byte limit"
            )));
        }
        if !(2..=MAX_OPTIONS).contains(&input.options.len()) {
            return Err(ToolError::InvalidInput(format!(
                "`options` must contain between 2 and {MAX_OPTIONS} choices"
            )));
        }
        let mut options = Vec::with_capacity(input.options.len());
        for (index, option) in input.options.into_iter().enumerate() {
            let option = option.trim();
            if option.is_empty() {
                return Err(ToolError::InvalidInput(format!(
                    "`options[{index}]` must not be empty"
                )));
            }
            if option.len() > MAX_OPTION_BYTES {
                return Err(ToolError::InvalidInput(format!(
                    "`options[{index}]` exceeds the {MAX_OPTION_BYTES}-byte limit"
                )));
            }
            if options
                .iter()
                .any(|existing: &String| existing.eq_ignore_ascii_case(option))
            {
                return Err(ToolError::InvalidInput(format!(
                    "`options[{index}]` duplicates an earlier choice"
                )));
            }
            options.push(option.to_string());
        }
        Ok(ElicitationRequest {
            question: question.to_string(),
            options,
        })
    }
}

#[async_trait]
impl Tool for AskUser {
    fn name(&self) -> &'static str {
        "ask_user"
    }

    fn description(&self) -> &'static str {
        "Ask the user one concise multiple-choice question when their decision is required."
    }

    fn schema(&self) -> Value {
        schema_for::<AskUserInput>()
    }

    fn approval_detail(&self, input: &Value) -> String {
        Self::request(input)
            .map(|request| detail_preview(&request.question))
            .unwrap_or_default()
    }

    fn effects(&self, input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        let _ = Self::request(input)?;
        Ok(Vec::new())
    }

    async fn invoke(&self, input: Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let request = Self::request(&input)?;
        match self.elicitor.ask(request).await {
            ElicitationOutcome::Answered(answer) => {
                let answer = answer.trim();
                if answer.is_empty() {
                    return Err(ToolError::Failed(
                        "the user response was empty; no choice was recorded".to_string(),
                    ));
                }
                if answer.len() > MAX_ANSWER_BYTES {
                    return Err(ToolError::Failed(format!(
                        "the user response exceeded the {MAX_ANSWER_BYTES}-byte limit"
                    )));
                }
                Ok(ToolOutput::ok(format!("User selected: {answer}")))
            }
            ElicitationOutcome::Cancelled => Ok(ToolOutput {
                text: "User cancelled the question.".to_string(),
                is_error: true,
                truncated: false,
                presentation: None,
            }),
        }
    }

    fn contract(&self) -> ToolContract {
        ToolContract {
            model_description:
                "Ask the user one concise multiple-choice question when their decision is required.",
            side_effect: SideEffectClass::ReadOnly,
            reversibility: Reversibility::Reversible,
            idempotency: Idempotency::Unknown,
            ..ToolContract::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_core::{ToolCall, ToolUseId};
    use localpilot_sandbox::{
        Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace,
    };
    use serde_json::json;

    struct Scripted(ElicitationOutcome);

    impl UserElicitor for Scripted {
        fn ask(
            &self,
            _request: ElicitationRequest,
        ) -> Pin<Box<dyn Future<Output = ElicitationOutcome> + Send + '_>> {
            Box::pin(std::future::ready(self.0.clone()))
        }
    }

    fn context(workspace: &Workspace) -> ToolContext<'_> {
        ToolContext {
            workspace,
            interactivity: Interactivity::Interactive,
            trusted: true,
            retention: None,
            processes: None,
            agents: None,
        }
    }

    #[tokio::test]
    async fn answered_and_cancelled_paths_are_explicit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = Workspace::new(dir.path()).expect("workspace");
        let input = json!({"question": "Pick one", "options": ["Red", "Blue"]});
        let answered = AskUser::new(Arc::new(Scripted(ElicitationOutcome::Answered(
            "Blue".to_string(),
        ))));
        let output = answered
            .invoke(input.clone(), &context(&workspace))
            .await
            .expect("answer");
        assert!(!output.is_error);
        assert_eq!(output.text, "User selected: Blue");

        let cancelled = AskUser::new(Arc::new(Scripted(ElicitationOutcome::Cancelled)));
        let output = cancelled
            .invoke(input, &context(&workspace))
            .await
            .expect("cancel");
        assert!(output.is_error);
        assert_eq!(output.text, "User cancelled the question.");
    }

    #[tokio::test]
    async fn registry_dispatch_redacts_the_answer_through_the_shared_chokepoint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = Workspace::new(dir.path()).expect("workspace");
        let secret = "sk-abcdefghijklmnopqrstuvwxyz0123";
        let mut registry = crate::ToolRegistry::new();
        registry.register(Box::new(AskUser::new(Arc::new(Scripted(
            ElicitationOutcome::Answered(secret.to_string()),
        )))));
        let result = registry
            .dispatch(
                &ToolCall::new(
                    ToolUseId::from("ask-1"),
                    "ask_user",
                    json!({"question": "Paste a value", "options": ["A", "B"]}),
                ),
                &context(&workspace),
                &PermissionEngine::new(Profile::Default, Vec::new()),
                &ScriptedApprover::new(Vec::new()),
            )
            .await;
        assert!(!result.output.contains(secret));
        assert!(result.output.contains("[REDACTED]"));
    }

    #[test]
    fn malformed_and_unbounded_inputs_are_rejected_before_elicitation() {
        let malformed = json!({"question": "Pick one", "options": ["only one"]});
        assert!(AskUser::request(&malformed).is_err());
        let duplicate = json!({"question": "Pick one", "options": ["Blue", "blue"]});
        assert!(AskUser::request(&duplicate).is_err());
        let oversized = json!({
            "question": "x".repeat(MAX_QUESTION_BYTES + 1),
            "options": ["Red", "Blue"]
        });
        assert!(AskUser::request(&oversized).is_err());
    }

    #[test]
    fn schema_is_generated_from_the_typed_input() {
        let tool = AskUser::new(Arc::new(Scripted(ElicitationOutcome::Cancelled)));
        let schema = tool.schema();
        let required = schema["required"].as_array().expect("required fields");
        assert!(required.contains(&json!("question")));
        assert!(required.contains(&json!("options")));
    }
}
