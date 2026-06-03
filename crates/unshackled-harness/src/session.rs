//! The agent-mode session runtime: the conversational loop both operating modes
//! share. It streams provider events, routes tool calls through the permission
//! engine, persists the transcript, and supports cancellation, loop limits, and
//! context compaction.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use unshackled_config::redact::redact;
use unshackled_core::{ContentBlock, Message, Role, SessionId, TokenUsage, ToolCall, ToolUseId};
use unshackled_llm::{
    ModelEvent, ModelEventStream, ModelProvider, ModelRequest, ProviderError, QuotaInfo, ToolSpec,
};
use unshackled_recovery::{
    detect, is_repeated_token_loop, is_slash_flood, ModelHealth, RecoveryEngine,
};
use unshackled_sandbox::{Approver, Interactivity, PermissionEngine};
use unshackled_store::Store;
use unshackled_tools::{ToolContext, ToolRegistry};

use crate::compaction::{compact_with_summary, estimate_tokens};

/// Why a turn loop stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The model produced a final answer.
    Done,
    /// The turn cap was reached.
    MaxTurns,
    /// The tool-call cap was reached.
    MaxToolCalls,
    /// The user cancelled.
    Cancelled,
    /// The provider/model was marked degraded by recovery.
    Degraded,
    /// The provider could not be reached.
    ProviderError,
}

/// A UI-agnostic runtime event. Consumers (print mode, the TUI) subscribe to a
/// broadcast channel so they share one event source.
#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    /// A chunk of final-answer text.
    Text(String),
    /// A chunk of reasoning. Metadata, never the final answer.
    Reasoning(String),
    /// A tool call started.
    ToolStarted { id: String, name: String },
    /// A tool call finished.
    ToolFinished { id: String, is_error: bool },
    /// Token usage.
    Usage(TokenUsage),
    /// Estimated context usage for the request about to be sent.
    ContextUsage { used: usize, limit: usize },
    /// A provider warning.
    Warning(String),
    /// The model updated the task plan shown to the user.
    Plan(Vec<PlanStep>),
    /// The provider rate-limited or exhausted quota; carries a human-readable
    /// description of when a retry is eligible, for the UI.
    QuotaPaused { reset: String },
    /// A recovery event occurred; model health is attached.
    Recovery { health: ModelHealth },
    /// The loop stopped.
    Stopped(StopReason),
}

/// One entry in the task plan the model maintains via the `update_plan` tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStep {
    pub title: String,
    pub status: String,
}

/// Tuning for a session.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub model: String,
    pub max_turns: u32,
    pub max_tool_calls: u32,
    pub interactivity: Interactivity,
    pub trusted: bool,
    pub context_token_limit: usize,
    /// How many times to retry a transient connection failure (network or
    /// 5xx) before giving up, with exponential backoff between attempts.
    pub max_stream_retries: u32,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            model: "default".to_string(),
            max_turns: 12,
            max_tool_calls: 24,
            interactivity: Interactivity::Interactive,
            trusted: true,
            context_token_limit: 24_000,
            max_stream_retries: 3,
        }
    }
}

const REPAIR_PROMPT: &str =
    "Your previous response was unusable. Stop, and produce a clean, well-formed reply.";

/// The agent-mode runtime.
pub struct SessionRuntime {
    provider: Arc<dyn ModelProvider>,
    tools: ToolRegistry,
    engine: PermissionEngine,
    approver: Box<dyn Approver>,
    store: Store,
    workspace: unshackled_sandbox::Workspace,
    recovery: RecoveryEngine,
    config: SessionConfig,
    session_id: SessionId,
    messages: Vec<Message>,
    /// Quota metadata from the most recent provider rate-limit/quota error in a
    /// turn, used to schedule a precise pause. Reset at the start of each turn.
    last_quota: Option<QuotaInfo>,
}

impl SessionRuntime {
    /// Build a runtime. `messages` may seed a system prompt.
    #[must_use]
    #[allow(clippy::too_many_arguments)] // a runtime genuinely composes these collaborators
    pub fn new(
        provider: Arc<dyn ModelProvider>,
        tools: ToolRegistry,
        engine: PermissionEngine,
        approver: Box<dyn Approver>,
        store: Store,
        workspace: unshackled_sandbox::Workspace,
        recovery: RecoveryEngine,
        config: SessionConfig,
        seed: Vec<Message>,
    ) -> Self {
        let mut messages = Vec::with_capacity(seed.len() + 1);
        messages.push(Message::text(
            Role::System,
            crate::system_prompt::agent_system_prompt(&tools),
        ));
        messages.extend(seed);

        Self {
            provider,
            tools,
            engine,
            approver,
            store,
            workspace,
            recovery,
            config,
            session_id: SessionId::new(),
            messages,
            last_quota: None,
        }
    }

    /// The session id (transcripts are stored under it).
    #[must_use]
    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// The current model health.
    #[must_use]
    pub fn health(&self) -> ModelHealth {
        self.recovery.health()
    }

    /// The store backing this session (for persisting paused-run state).
    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Quota metadata from the last provider rate-limit/quota error this turn,
    /// if any. Consulted after a [`StopReason::ProviderError`] to size the pause.
    #[must_use]
    pub fn last_quota(&self) -> Option<&QuotaInfo> {
        self.last_quota.as_ref()
    }

    /// Seed a system message into the conversation — for example retrieved
    /// project context injected by the host before a turn. Persisted and counted
    /// in context like any message.
    pub fn seed_system(&mut self, text: impl Into<String>) {
        self.append(Message::new(Role::System, vec![ContentBlock::text(text)]));
    }

    /// Open a provider stream, retrying a transient connection failure (network
    /// or 5xx) up to `max_stream_retries` with exponential backoff. A rate-limit
    /// or quota error is not retried here — it pauses the run instead.
    async fn open_stream(
        &mut self,
        request: &ModelRequest,
        events: &broadcast::Sender<RuntimeEvent>,
        cancel: &CancellationToken,
    ) -> Result<ModelEventStream, StreamOpen> {
        let max = self.config.max_stream_retries;
        let mut attempt: u32 = 0;
        loop {
            match self.provider.stream(request.clone()).await {
                Ok(stream) => return Ok(stream),
                Err(err) => {
                    self.last_quota = err.quota().cloned();
                    let transient = matches!(
                        err,
                        ProviderError::Network(_) | ProviderError::Server { .. }
                    );
                    if transient && attempt < max {
                        attempt += 1;
                        let secs = 1u64 << (attempt - 1).min(5);
                        let _ = events.send(RuntimeEvent::Warning(format!(
                            "provider unreachable ({err}); retry {attempt}/{max} in {secs}s"
                        )));
                        tokio::select! {
                            _ = cancel.cancelled() => return Err(StreamOpen::Cancelled),
                            _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
                        }
                    } else {
                        if let Some(reset) = self.last_quota.as_ref().map(quota_reset_label) {
                            let _ = events.send(RuntimeEvent::QuotaPaused { reset });
                        }
                        let _ = events.send(RuntimeEvent::Warning(err.to_string()));
                        return Err(StreamOpen::Failed);
                    }
                }
            }
        }
    }

    fn tool_specs(&self) -> Vec<ToolSpec> {
        self.tools
            .specs()
            .into_iter()
            .map(|(name, description, input_schema)| ToolSpec {
                name: name.to_string(),
                description: description.to_string(),
                input_schema,
            })
            .collect()
    }

    fn append(&mut self, message: Message) {
        // Persist (redacting) before keeping it in memory; a write failure is
        // logged but does not crash the loop.
        if let Err(err) = self.store.append_message(self.session_id, &message) {
            tracing::warn!(error = %err, "failed to persist transcript message");
        }
        self.messages.push(message);
    }

    /// Run one user turn to completion. Streaming and tool execution are
    /// cancellable; on cancellation no partial message is persisted, so the
    /// transcript stays consistent.
    pub async fn run_turn(
        &mut self,
        user_input: &str,
        events: &broadcast::Sender<RuntimeEvent>,
        cancel: &CancellationToken,
    ) -> StopReason {
        self.append(Message::text(Role::User, user_input));
        self.last_quota = None;
        let mut tool_calls_used = 0u32;

        for _ in 0..self.config.max_turns {
            if cancel.is_cancelled() {
                return self.stop(events, StopReason::Cancelled);
            }

            let compacted =
                compact_with_summary(self.messages.clone(), self.config.context_token_limit);
            let used = estimate_tokens(&compacted.messages);
            let _ = events.send(RuntimeEvent::ContextUsage {
                used,
                limit: self.config.context_token_limit,
            });
            let request = ModelRequest::new(self.config.model.clone(), compacted.messages)
                .with_tools(self.tool_specs());

            let mut stream = match self.open_stream(&request, events, cancel).await {
                Ok(stream) => stream,
                Err(StreamOpen::Cancelled) => return self.stop(events, StopReason::Cancelled),
                Err(StreamOpen::Failed) => return self.stop(events, StopReason::ProviderError),
            };

            let mut text = String::new();
            let mut reasoning = String::new();
            let mut calls: Vec<(String, String, serde_json::Value)> = Vec::new();
            let mut stream_failed = false;
            // Byte length at the last live degenerate-output check; re-checked
            // every `FLOOD_CHECK_STRIDE` bytes so a runaway stream is aborted
            // early instead of flooding the whole turn.
            let mut last_flood_check = 0usize;
            const FLOOD_CHECK_STRIDE: usize = 32;

            loop {
                tokio::select! {
                    () = cancel.cancelled() => {
                        return self.stop(events, StopReason::Cancelled);
                    }
                    event = stream.next() => match event {
                        Some(Ok(ModelEvent::TextDelta(delta))) => {
                            let _ = events.send(RuntimeEvent::Text(delta.clone()));
                            text.push_str(&delta);
                            // Live guard: stop a degenerate punctuation flood or a
                            // repeated-token loop early; the post-stream recovery
                            // ladder then handles the bad turn.
                            if text.len().saturating_sub(last_flood_check) >= FLOOD_CHECK_STRIDE {
                                last_flood_check = text.len();
                                if is_slash_flood(&text) || is_repeated_token_loop(&text) {
                                    let _ = events.send(RuntimeEvent::Warning(
                                        "degenerate output detected; stopping generation"
                                            .to_string(),
                                    ));
                                    break;
                                }
                            }
                        }
                        Some(Ok(ModelEvent::ReasoningDelta(delta))) => {
                            let _ = events.send(RuntimeEvent::Reasoning(delta.clone()));
                            reasoning.push_str(&delta);
                        }
                        Some(Ok(ModelEvent::ToolCall { id, name, input_json })) => {
                            calls.push((id, name, input_json));
                        }
                        Some(Ok(ModelEvent::Usage(usage))) => {
                            let _ = events.send(RuntimeEvent::Usage(usage));
                        }
                        Some(Ok(ModelEvent::ProviderWarning { message })) => {
                            let _ = events.send(RuntimeEvent::Warning(message));
                        }
                        Some(Ok(ModelEvent::Done)) => break,
                        Some(Ok(_)) => {}
                        Some(Err(err)) => {
                            self.last_quota = err.quota().cloned();
                            let _ = events
                                .send(RuntimeEvent::Warning(format!("stream error: {err}")));
                            stream_failed = true;
                            break;
                        }
                        None => break,
                    }
                }
            }

            // Bad-output detection and recovery.
            let bad = if stream_failed {
                Some(unshackled_recovery::BadOutputKind::MalformedStructuredOutput)
            } else {
                detect(&text, !calls.is_empty())
            };
            if let Some(kind) = bad {
                let diagnostic = self.recovery.record_bad_turn(kind);
                self.persist_recovery(&diagnostic);
                let _ = events.send(RuntimeEvent::Recovery {
                    health: self.recovery.health(),
                });
                if self.recovery.health() == ModelHealth::Degraded {
                    return self.stop(events, StopReason::Degraded);
                }
                self.messages.push(Message::text(Role::User, REPAIR_PROMPT));
                continue;
            }
            self.recovery.record_clean_turn();

            // Assemble and persist the assistant message.
            let mut content = Vec::new();
            if !reasoning.is_empty() {
                content.push(ContentBlock::Reasoning {
                    text: reasoning,
                    signature: None,
                    provider_metadata: None,
                });
            }
            if !text.is_empty() {
                content.push(ContentBlock::text(text));
            }
            for (id, name, input) in &calls {
                content.push(ContentBlock::ToolUse(ToolCall::new(
                    ToolUseId::from(id.as_str()),
                    name.clone(),
                    input.clone(),
                )));
            }
            self.append(Message::new(Role::Assistant, content));

            if calls.is_empty() {
                return self.stop(events, StopReason::Done);
            }

            if let Some(message) = invalid_tool_calls(&calls) {
                let _ = events.send(RuntimeEvent::Warning(message.clone()));
                self.messages.push(Message::text(Role::User, message));
                continue;
            }

            // Execute tool calls through the permission-gated registry.
            for (id, name, input) in calls {
                if tool_calls_used >= self.config.max_tool_calls {
                    return self.stop(events, StopReason::MaxToolCalls);
                }
                tool_calls_used += 1;

                // Surface the task plan to the UI as the model updates it.
                if name == "update_plan" {
                    if let Some(steps) = parse_plan(&input) {
                        let _ = events.send(RuntimeEvent::Plan(steps));
                    }
                }

                let _ = events.send(RuntimeEvent::ToolStarted {
                    id: id.clone(),
                    name: name.clone(),
                });
                let call = ToolCall::new(ToolUseId::from(id.as_str()), name, input);
                let ctx = ToolContext {
                    workspace: &self.workspace,
                    interactivity: self.config.interactivity,
                    trusted: self.config.trusted,
                };
                let result = self
                    .tools
                    .dispatch(&call, &ctx, &self.engine, self.approver.as_ref())
                    .await;
                let _ = events.send(RuntimeEvent::ToolFinished {
                    id: result.id.to_string(),
                    is_error: result.is_error,
                });
                self.append(Message::new(
                    Role::Tool,
                    vec![ContentBlock::ToolResult(result)],
                ));
            }
        }

        self.stop(events, StopReason::MaxTurns)
    }

    fn stop(&self, events: &broadcast::Sender<RuntimeEvent>, reason: StopReason) -> StopReason {
        let _ = events.send(RuntimeEvent::Stopped(reason));
        reason
    }

    fn persist_recovery(&self, diagnostic: &unshackled_recovery::RecoveryDiagnostic) {
        if let Ok(json) = serde_json::to_string(diagnostic) {
            let key = format!("recovery-{}", self.session_id);
            // Stored as a tool-output-style snapshot; redaction is applied by the
            // store and again here for defense in depth.
            let _ = self.store.put_tool_output(&key, &redact(&json));
        }
    }
}

fn invalid_tool_calls(calls: &[(String, String, serde_json::Value)]) -> Option<String> {
    for (id, name, input) in calls {
        if id.trim().is_empty() {
            return Some(
                "Tool call error: missing tool-call id. Retry with a valid id.".to_string(),
            );
        }
        if name.trim().is_empty() {
            return Some(
                "Tool call error: missing tool name. Retry with a registered tool name."
                    .to_string(),
            );
        }
        if !input.is_object() {
            return Some(format!(
                "Tool call error for {name}: input must be a JSON object matching the tool schema."
            ));
        }
    }
    None
}

/// Parse the `update_plan` tool input into plan steps. Lenient: a malformed or
/// partial entry is skipped rather than failing the turn.
fn parse_plan(input: &serde_json::Value) -> Option<Vec<PlanStep>> {
    let steps = input.get("steps")?.as_array()?;
    let parsed: Vec<PlanStep> = steps
        .iter()
        .filter_map(|step| {
            let title = step.get("title")?.as_str()?.to_string();
            let status = step
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("pending")
                .to_string();
            Some(PlanStep { title, status })
        })
        .collect();
    Some(parsed)
}

/// The outcome of failing to open a provider stream after retries.
enum StreamOpen {
    /// The user cancelled during a retry backoff.
    Cancelled,
    /// The error was non-transient or retries were exhausted.
    Failed,
}

/// A short, human-readable description of when a rate-limited request becomes
/// eligible to retry, from the most specific metadata the provider supplied.
fn quota_reset_label(quota: &QuotaInfo) -> String {
    if let Some(retry_after) = quota.retry_after {
        format!("retry in ~{}s", retry_after.as_secs())
    } else if let Some(reset_at) = quota.reset_at {
        format!("resets at {reset_at}")
    } else if let Some(kind) = &quota.limit_kind {
        format!("{kind} limit reached")
    } else {
        "rate limited".to_string()
    }
}
