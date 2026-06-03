//! The agent-mode session runtime: the conversational loop both operating modes
//! share. It streams provider events, routes tool calls through the permission
//! engine, persists the transcript, and supports cancellation, loop limits, and
//! context compaction.

use std::sync::Arc;

use futures::StreamExt;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use unshackled_config::redact::redact;
use unshackled_core::{ContentBlock, Message, Role, SessionId, TokenUsage, ToolCall, ToolUseId};
use unshackled_llm::{ModelEvent, ModelProvider, ModelRequest, QuotaInfo, ToolSpec};
use unshackled_recovery::{detect, ModelHealth, RecoveryEngine};
use unshackled_sandbox::{Approver, Interactivity, PermissionEngine};
use unshackled_store::Store;
use unshackled_tools::{ToolContext, ToolRegistry};

use crate::compaction::compact;

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
    /// A provider warning.
    Warning(String),
    /// A recovery event occurred; model health is attached.
    Recovery { health: ModelHealth },
    /// The loop stopped.
    Stopped(StopReason),
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
            messages: seed,
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

            let request = ModelRequest::new(
                self.config.model.clone(),
                compact(self.messages.clone(), self.config.context_token_limit),
            )
            .with_tools(self.tool_specs());

            let mut stream = match self.provider.stream(request).await {
                Ok(stream) => stream,
                Err(err) => {
                    self.last_quota = err.quota().cloned();
                    let _ = events.send(RuntimeEvent::Warning(err.to_string()));
                    return self.stop(events, StopReason::ProviderError);
                }
            };

            let mut text = String::new();
            let mut reasoning = String::new();
            let mut calls: Vec<(String, String, serde_json::Value)> = Vec::new();
            let mut stream_failed = false;

            loop {
                tokio::select! {
                    () = cancel.cancelled() => {
                        return self.stop(events, StopReason::Cancelled);
                    }
                    event = stream.next() => match event {
                        Some(Ok(ModelEvent::TextDelta(delta))) => {
                            let _ = events.send(RuntimeEvent::Text(delta.clone()));
                            text.push_str(&delta);
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
                        Some(Err(_)) => {
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

            // Execute tool calls through the permission-gated registry.
            for (id, name, input) in calls {
                if tool_calls_used >= self.config.max_tool_calls {
                    return self.stop(events, StopReason::MaxToolCalls);
                }
                tool_calls_used += 1;

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
