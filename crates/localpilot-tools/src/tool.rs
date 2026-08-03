//! The tool trait, execution context, and output type.

use async_trait::async_trait;
use localpilot_sandbox::{Effect, Interactivity, Workspace};
use serde_json::Value;

use crate::error::ToolError;
use localpilot_core::ToolOutcome;

/// Context passed to a tool: the workspace it may touch and how the session runs.
pub struct ToolContext<'a> {
    pub workspace: &'a Workspace,
    pub interactivity: Interactivity,
    pub trusted: bool,
    /// Where oversized tool output spills, keyed by an opaque id the model can
    /// pass to `read_tool_output`. `None` disables spilling (output is capped
    /// only).
    pub retention: Option<&'a dyn OutputRetention>,
    /// The session-scoped registry of background processes that `run_background`
    /// starts and manages. `None` disables background processes (the host wired
    /// no registry), and `run_background` reports them as unavailable.
    pub processes: Option<&'a crate::builtins_background::BackgroundProcesses>,
    /// The host that can run a subagent for `delegate`. `None` means this
    /// session has no delegation surface (no definitions loaded, or a host that
    /// wired none), and `delegate` reports itself unavailable rather than
    /// failing obscurely.
    pub agents: Option<&'a dyn AgentHost>,
    /// The host that can put a question to the user for `ask_user`. `None` means
    /// there is no human on this session — a piped run, a CI run, a subagent —
    /// and `ask_user` says so rather than waiting for an answer that cannot come.
    pub prompter: Option<&'a dyn UserPrompter>,
    /// The host that can reach this session's swarm peers. `None` means this
    /// session is not collaborating — the overwhelmingly common case — and the
    /// swarm tool reports itself unavailable instead of failing obscurely.
    pub peers: Option<&'a dyn SwarmPeers>,
}

/// Who this session is inside its swarm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwarmIdentity {
    /// This session's id, as text.
    pub session: String,
    /// The name peers address it by.
    pub name: String,
    /// Whether it may address the whole swarm rather than only its own subtree.
    pub is_coordinator: bool,
}

/// One other member, as the model sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSummary {
    /// The peer's session id, as text.
    pub session: String,
    /// The peer's name.
    pub name: String,
    /// `coordinator`, `worker`, or `peer`.
    pub role: String,
    /// Where the peer is in its life, in a word.
    pub status: String,
    /// Whether the peer is inside this session's own subtree — the scope a
    /// non-coordinator may broadcast to.
    pub in_my_subtree: bool,
}

/// Who a message is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Audience {
    /// One peer, named by session id or by an unambiguous name.
    One(String),
    /// Everything below this session in the spawn tree, this session excluded.
    Subtree,
    /// Every member of the swarm. Coordinator-only.
    Swarm,
}

/// How urgently a message should reach its recipient.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Delivery {
    /// Record it for attached clients; do not disturb a running turn. The
    /// cheapest option, and the right one for anything the recipient does not
    /// have to act on.
    #[default]
    Notify,
    /// Put it into the recipient's running turn at its next safe boundary.
    Interrupt,
    /// Interrupt if the recipient is busy; start a turn if it is idle. For a
    /// message that is useless unless it is acted on.
    Wake,
}

/// A message one member is sending to others.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerMessage {
    /// Who it is for.
    pub audience: Audience,
    /// A one-line summary. Required for a long body: a recipient mid-task needs
    /// to decide whether to read the rest *before* reading the rest.
    pub tldr: Option<String>,
    /// The message.
    pub body: String,
    /// How urgently it should land.
    pub delivery: Delivery,
}

/// What sending resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivered {
    /// How many members it reached.
    pub reached: usize,
    /// Their names, for the sender's record.
    pub recipients: Vec<String>,
}

/// The host side of agent-to-agent messaging.
///
/// A tool cannot route a message by itself: it has no registry, no spawn tree,
/// and no way to reach another session's turn. The host implements this and
/// hands it in through [`ToolContext`], exactly as it does for delegation and
/// user prompts, so the swarm tool stays an ordinary tool with no special path
/// through the registry.
///
/// Every method is async, including the two that merely read. They reach state
/// behind the server's async locks, and a synchronous signature would force
/// either a blocking call inside the runtime — which panics outright on a
/// current-thread one — or a cache that goes stale exactly when the swarm is
/// changing.
pub trait SwarmPeers: Send + Sync {
    /// Who this session is.
    fn identity<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = SwarmIdentity> + Send + 'a>>;

    /// Every other member of this session's swarm.
    fn roster<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<PeerSummary>> + Send + 'a>>;

    /// Deliver `message`, or explain why it could not be.
    ///
    /// # Errors
    /// A model-readable reason: an unknown or ambiguous recipient, or an
    /// audience this session is not allowed to address.
    fn send<'a>(
        &'a self,
        message: &'a PeerMessage,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Delivered, String>> + Send + 'a>>;
}

/// The host side of delegation.
///
/// A tool cannot spawn a session by itself — it has no provider, no registry,
/// and no permission engine. The host implements this and hands it in through
/// [`ToolContext`], exactly as it does for output retention and background
/// processes, so `delegate` stays an ordinary tool with no special path through
/// the registry.
pub trait AgentHost: Send + Sync {
    /// Every agent this session can delegate to: `(name, description)`.
    fn available(&self) -> Vec<(String, String)>;

    /// Run `agent` against `task` and resolve to its bounded summary, or to a
    /// message explaining why it could not run.
    fn run<'a>(
        &'a self,
        agent: &'a str,
        task: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>;
}

/// One selectable answer to a [`UserQuestion`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionOption {
    /// The text the user picks.
    pub label: String,
    /// What choosing it means, when the label alone is not enough.
    pub description: Option<String>,
}

/// A question to put to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserQuestion {
    /// A short chip-style label for the question's topic.
    pub header: Option<String>,
    /// The question itself.
    pub question: String,
    /// The offered answers. The model's guess at the answer space — the user is
    /// the authority on it, which is why free text is always also offered.
    pub options: Vec<QuestionOption>,
    /// Whether several options may be chosen together.
    pub multi_select: bool,
}

/// How the user answered one [`UserQuestion`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserAnswer {
    /// One or more offered labels.
    Selected(Vec<String>),
    /// Free text the user typed instead.
    Other(String),
    /// The user dismissed the question. Not a failure: the model is told to use
    /// its own judgment and say so.
    Dismissed,
}

/// The host side of asking the user a question.
///
/// A tool cannot reach the user by itself — it has no terminal and no event
/// loop. The host implements this and hands it in through [`ToolContext`],
/// exactly as it does for delegation, so `ask_user` stays an ordinary tool with
/// no special path through the registry. The interactive front-end suspends the
/// turn while it prompts, as the approval gate already does.
pub trait UserPrompter: Send + Sync {
    /// Ask `questions` in order and resolve to one answer each.
    fn ask<'a>(
        &'a self,
        questions: &'a [UserQuestion],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<UserAnswer>> + Send + 'a>>;
}

/// A tighten-only gate consulted after the permission engine for every tool
/// call. A gate can only `Pass` or `Block` — it can never grant what the
/// engine refused, so hooks extend the safety model without ever weakening
/// it. The permission engine itself is the always-on first link of this
/// chain and is not removable.
pub trait ToolGate: Send + Sync {
    /// A stable name, recorded with any block verdict.
    fn name(&self) -> &str;

    /// Inspect a call (after its effects were resolved and authorized by the
    /// engine) and either let it proceed or block it with a model-visible
    /// reason.
    fn check(&self, call: &localpilot_core::ToolCall, effects: &[Effect]) -> GateVerdict;
}

/// A gate's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateVerdict {
    /// Let the call proceed to the next gate (or execution).
    Pass,
    /// Refuse the call with a model-visible reason.
    Block { reason: String },
}

/// A sink for full tool outputs that were too large to keep in context. The
/// host wires its store in; the registry spills, and `read_tool_output`
/// fetches.
pub trait OutputRetention: Send + Sync {
    /// Retain `output` under `id`, replacing any previous value.
    ///
    /// # Errors
    /// Returns a human-readable reason when the output cannot be retained.
    fn retain(&self, id: &str, output: &str) -> Result<(), String>;

    /// Fetch the retained output for `id`, or `None` if absent.
    ///
    /// # Errors
    /// Returns a human-readable reason when the lookup fails.
    fn fetch(&self, id: &str) -> Result<Option<String>, String>;
}

/// A tool's textual result, before redaction and the final id are attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub text: String,
    pub outcome: ToolOutcome,
    pub truncated: bool,
    pub presentation: Option<ToolOutputPresentation>,
    /// What this call touched, reported by the tool itself.
    ///
    /// Empty for the overwhelming majority of tools, which touch no file. A
    /// file-mutating tool attaches one entry per file it changed, with the line
    /// range where it knows one. Nothing downstream infers or parses this — the
    /// tool is the only thing that knows what it did, and it knows exactly.
    pub touches: Vec<crate::touch::FileTouch>,
}

/// Typed host-facing output retained alongside the ordinary model-facing text.
/// The registry applies the same redaction boundary to both projections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOutputPresentation {
    Shell(ShellOutput),
}

/// Captured shell streams and process status before any UI formatting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl ToolOutput {
    /// A successful output.
    #[must_use]
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            outcome: ToolOutcome::Ok,
            truncated: false,
            presentation: None,
            touches: Vec::new(),
        }
    }

    /// A successful output marked as truncated.
    #[must_use]
    pub fn truncated(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            outcome: ToolOutcome::Ok,
            truncated: true,
            presentation: None,
            touches: Vec::new(),
        }
    }

    /// The same output with `outcome` replaced, preserving the truncation
    /// marker — the assignment sites all refine a value returned by a capping
    /// helper.
    #[must_use]
    pub fn with_outcome(mut self, outcome: ToolOutcome) -> Self {
        self.outcome = outcome;
        self
    }

    /// Report that this call touched a file.
    #[must_use]
    pub fn touching(mut self, touch: crate::touch::FileTouch) -> Self {
        self.touches.push(touch);
        self
    }

    /// Report several touches at once — one call, several files or several
    /// ranges. The case a per-call inference would get wrong.
    #[must_use]
    pub fn touching_all(
        mut self,
        touches: impl IntoIterator<Item = crate::touch::FileTouch>,
    ) -> Self {
        self.touches.extend(touches);
        self
    }

    /// Whether the model sees this output as an error.
    #[must_use]
    pub fn is_error(&self) -> bool {
        self.outcome.is_error()
    }
}

/// A builtin tool. Object-safe so the registry can hold `Box<dyn Tool>`.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The tool's stable name as exposed to the model.
    fn name(&self) -> &str;

    /// A one-line description.
    fn description(&self) -> &str;

    /// The JSON schema for this tool's input, generated from a typed struct.
    fn schema(&self) -> Value;

    /// The side effects this call will have, used to drive the permission engine.
    /// Resolving effects must not itself perform the effect.
    ///
    /// # Errors
    /// Returns [`ToolError::InvalidInput`] if the input does not parse.
    fn effects(&self, input: &Value, ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError>;

    /// A short, human-readable description of the concrete target this call
    /// acts on (the command line, path, or query), shown in approval prompts so
    /// the user sees *what* they are approving. Display-only — never an input
    /// to a permission decision. Every tool with side effects must supply one;
    /// the default empty string is acceptable only for effect-free tools.
    fn approval_detail(&self, input: &Value) -> String {
        let _ = input;
        String::new()
    }

    /// Execute the tool. Only called after every effect has been authorized.
    ///
    /// # Errors
    /// Returns [`ToolError`] on invalid input or execution failure.
    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError>;

    /// Static discipline metadata: side effects, reversibility, pre/post-
    /// conditions, and how the result is verified. Additive and advisory — the
    /// default is an empty contract, and the permission path is unaffected by it.
    fn contract(&self) -> crate::contract::ToolContract {
        crate::contract::ToolContract::default()
    }
}

/// Parse a tool's JSON input into a typed struct.
///
/// # Errors
/// Returns [`ToolError::InvalidInput`] if deserialization fails.
pub(crate) fn parse_input<T: serde::de::DeserializeOwned>(input: &Value) -> Result<T, ToolError> {
    serde_json::from_value(input.clone()).map_err(|e| ToolError::InvalidInput(e.to_string()))
}

/// Generate a JSON schema value from a typed input struct.
pub(crate) fn schema_for<T: schemars::JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null)
}

/// Bound an approval-prompt detail string to a displayable length.
pub(crate) fn detail_preview(text: &str) -> String {
    const MAX_CHARS: usize = 160;
    let trimmed = text.trim();
    let mut shown: String = trimmed.chars().take(MAX_CHARS).collect();
    if trimmed.chars().count() > MAX_CHARS {
        shown.push('…');
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::SideEffectClass;
    use localpilot_sandbox::Effect;

    /// A tool that overrides nothing beyond the required methods, to prove the
    /// default contract path.
    struct NoopTool;

    #[async_trait]
    impl Tool for NoopTool {
        fn name(&self) -> &str {
            "noop"
        }
        fn description(&self) -> &str {
            ""
        }
        fn schema(&self) -> Value {
            Value::Null
        }
        fn effects(
            &self,
            _input: &Value,
            _ctx: &ToolContext<'_>,
        ) -> Result<Vec<Effect>, ToolError> {
            Ok(Vec::new())
        }
        async fn invoke(
            &self,
            _input: Value,
            _ctx: &ToolContext<'_>,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::ok(""))
        }
    }

    #[test]
    fn a_tool_without_an_override_reports_the_default_contract() {
        let contract = NoopTool.contract();
        assert_eq!(contract.side_effect, SideEffectClass::ReadOnly);
        assert!(contract.preconditions.is_empty());
        assert!(contract.postconditions.is_empty());
        assert!(!contract.has_side_effect());
    }
}
