//! The agent-mode session runtime: the conversational loop both operating modes
//! share. It streams provider events, routes tool calls through the permission
//! engine, persists the transcript, and supports cancellation, loop limits, and
//! context compaction.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use localpilot_config::redact::redact;
use localpilot_config::CheckConfig;
use localpilot_core::{
    ContentBlock, EventId, Message, Role, SessionId, TokenUsage, ToolCall, ToolUseId,
};
use localpilot_llm::{
    ModelEvent, ModelEventStream, ModelProvider, ModelRequest, ProviderError, QuotaInfo, ToolSpec,
};
use localpilot_recovery::{detect, ModelHealth, RecoveryEngine, StreamMonitor};
use localpilot_sandbox::{Approver, Interactivity, PermissionEngine, Profile};
use localpilot_store::{origin_for, transcript_from_events, OpenReason, SessionEventKind, Store};
use localpilot_tools::{ToolContext, ToolRegistry};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::compaction::{compact_with_summary, estimate_tokens, CompactionResult};
use crate::hooks::{HookEvent, HookFabric};
use crate::quality::{CheckOutcome, CheckRunner};
use crate::rules::{trigger_for_cadence, Trigger};

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
    ToolFinished {
        id: String,
        name: String,
        is_error: bool,
        output: String,
    },
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

/// Result of manually compacting the runtime message history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManualCompaction {
    /// Whether older messages were removed and summarized.
    pub compacted: bool,
    /// Estimated context usage after compaction.
    pub context_used: usize,
    /// Configured context limit used for the operation.
    pub context_limit: usize,
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
    /// Requested reasoning effort for provider turns; mapped (or no-op
    /// clamped) per provider. Switchable mid-session.
    pub reasoning_effort: Option<localpilot_llm::ReasoningEffort>,
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
            reasoning_effort: None,
            max_stream_retries: 3,
        }
    }
}

/// Tokens held back from the model's context window for the response and
/// protocol overhead when deriving the session budget from a real window.
const CONTEXT_RESERVE_TOKENS: usize = 4_096;

/// The session's effective context budget: the model's real window minus a
/// response reserve when the window is known (per-provider `context_window`
/// or discovery), otherwise the configured global limit. Estimates feeding
/// this budget are the bytes/4 heuristic — see docs/providers.md for its bias.
#[must_use]
pub fn effective_context_limit(window: Option<u64>, configured: usize) -> usize {
    match window {
        Some(window) => {
            let window = usize::try_from(window).unwrap_or(usize::MAX);
            window
                .saturating_sub(CONTEXT_RESERVE_TOKENS)
                .max(CONTEXT_RESERVE_TOKENS)
        }
        None => configured,
    }
}

/// A thread-safe queue of steering input: user text typed while a turn is
/// running, admitted at the next safe provider-turn boundary (after the
/// current iteration's tool calls, before the next provider call).
#[derive(Debug, Clone, Default)]
pub struct SteerQueue(Arc<std::sync::Mutex<std::collections::VecDeque<String>>>);

impl SteerQueue {
    /// Queue steering text for the running turn.
    pub fn push(&self, text: impl Into<String>) {
        if let Ok(mut queue) = self.0.lock() {
            queue.push_back(text.into());
        }
    }

    /// Whether anything is queued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.lock().map(|q| q.is_empty()).unwrap_or(true)
    }

    fn drain(&self) -> Vec<String> {
        self.0
            .lock()
            .map(|mut queue| queue.drain(..).collect())
            .unwrap_or_default()
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
    workspace: localpilot_sandbox::Workspace,
    recovery: RecoveryEngine,
    config: SessionConfig,
    session_id: SessionId,
    messages: Vec<Message>,
    /// Quota metadata from the most recent provider rate-limit/quota error in a
    /// turn, used to schedule a precise pause. Reset at the start of each turn.
    last_quota: Option<QuotaInfo>,
    /// Tail of the durable event log, for parent chaining.
    last_event: Option<EventId>,
    /// Bumped on every mutation of `messages`; keys the compaction cache.
    history_generation: u64,
    /// The compaction result for the current `history_generation`, so the
    /// per-iteration request shaping does not recompact unchanged history.
    compaction_cache: Option<(u64, CompactionResult)>,
    /// Steering input queued by the host while a turn runs.
    steer: SteerQueue,
    /// Registered lifecycle observers, context hooks, and tool gates.
    hooks: HookFabric,
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
        workspace: localpilot_sandbox::Workspace,
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

        let mut runtime = Self {
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
            last_event: None,
            history_generation: 0,
            compaction_cache: None,
            steer: SteerQueue::default(),
            hooks: HookFabric::default(),
        };
        runtime.record_event(SessionEventKind::SessionOpened {
            reason: OpenReason::New,
        });
        runtime
    }

    /// Append one entry to the durable session event log, chaining it to the
    /// previous entry. A write failure is logged but never crashes the loop —
    /// the event log is an audit record, not a gate.
    pub fn record_event(&mut self, kind: SessionEventKind) {
        match self
            .store
            .append_event(self.session_id, self.last_event, kind)
        {
            Ok(id) => self.last_event = Some(id),
            Err(err) => tracing::warn!(error = %err, "failed to persist session event"),
        }
    }

    /// The id of the most recent durable event, for fork bookkeeping.
    #[must_use]
    pub fn last_event_id(&self) -> Option<EventId> {
        self.last_event
    }

    /// Record that this session is closing.
    pub fn close(&mut self) {
        self.record_event(SessionEventKind::SessionClosed);
    }

    /// Start a fresh session: a new id, a clean conversation (the setup
    /// system prompt is kept), and a new durable event chain.
    pub fn start_new_session(&mut self) {
        self.clear_conversation();
        self.session_id = SessionId::new();
        self.last_event = None;
        self.record_event(SessionEventKind::SessionOpened {
            reason: OpenReason::New,
        });
    }

    /// Resume `session` from its durable event log: the conversation is
    /// rebuilt from the log (resume, replay, and audit are one mechanism) and
    /// new events chain onto its tail. The runtime's *current* permission
    /// profile and trust state stay in force — nothing from the resumed log
    /// can carry over stale elevated permissions.
    ///
    /// # Errors
    /// Returns the store error if the session's event log cannot be read.
    pub fn load_session(&mut self, session: SessionId) -> Result<(), localpilot_store::StoreError> {
        let events = self.store.read_events(session)?;
        let transcript = transcript_from_events(&events);
        // Keep the current setup prompt; the transcript never contains it.
        let setup = self
            .messages
            .first()
            .filter(|message| message.role == Role::System)
            .cloned();
        self.session_id = session;
        self.last_event = events.last().map(|event| event.id);
        self.messages = setup.into_iter().chain(transcript).collect();
        self.last_quota = None;
        self.history_generation += 1;
        self.compaction_cache = None;
        self.record_event(SessionEventKind::SessionOpened {
            reason: OpenReason::Resumed,
        });
        Ok(())
    }

    /// Branch the current conversation into a new session. The new session's
    /// log is self-contained (the history is re-recorded into it); with
    /// `mark_fork` it also records where it branched from, distinguishing a
    /// fork (a divergence point) from a plain clone.
    ///
    /// # Errors
    /// Returns the store error if the new session's log cannot be written.
    pub fn fork_session(
        &mut self,
        mark_fork: bool,
    ) -> Result<SessionId, localpilot_store::StoreError> {
        let fork_point = self.last_event;
        let history: Vec<Message> = self.messages.iter().skip(1).cloned().collect();
        self.session_id = SessionId::new();
        self.last_event = None;
        self.record_event(SessionEventKind::SessionOpened {
            reason: OpenReason::Forked,
        });
        if mark_fork {
            if let Some(from) = fork_point {
                self.record_event(SessionEventKind::BranchForked { from });
            }
        }
        for message in &history {
            self.store.append_message(self.session_id, message)?;
            self.record_event(SessionEventKind::Message {
                origin: origin_for(message),
                message: message.clone(),
            });
        }
        self.history_generation += 1;
        self.compaction_cache = None;
        Ok(self.session_id)
    }

    /// Run a user-initiated shell command through the permission engine. The
    /// run always lands in the durable event log; unless
    /// `exclude_from_context` is set, the command and its output are also
    /// surfaced into the transcript as a [`Role::UserShell`] message so the
    /// model can see what the user ran. With `exclude_from_context` the model
    /// context is untouched — the run remains auditable in the event log only.
    pub async fn run_user_shell(
        &mut self,
        program: &str,
        args: &[String],
        exclude_from_context: bool,
    ) -> localpilot_core::ToolResult {
        let call_id = format!("user-shell-{}", EventId::new());
        let call = ToolCall::new(
            ToolUseId::from(call_id.as_str()),
            "run_shell",
            serde_json::json!({ "program": program, "args": args }),
        );
        self.record_event(SessionEventKind::ToolStarted {
            id: call_id.clone(),
            name: "run_shell".to_string(),
        });
        let retention = StoreRetention(&self.store);
        let ctx = ToolContext {
            workspace: &self.workspace,
            interactivity: self.config.interactivity,
            trusted: self.config.trusted,
            retention: Some(&retention),
        };
        let result = self
            .tools
            .dispatch(&call, &ctx, &self.engine, self.approver.as_ref())
            .await;
        self.record_event(SessionEventKind::ToolFinished {
            id: call_id,
            name: "run_shell".to_string(),
            is_error: result.is_error,
        });
        if !exclude_from_context {
            let rendered = if args.is_empty() {
                format!("$ {program}\n{}", result.output)
            } else {
                format!("$ {program} {}\n{}", args.join(" "), result.output)
            };
            self.append(Message::text(Role::UserShell, rendered));
        }
        result
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

    /// Replace the active permission profile for subsequent turns. Interactive
    /// hosts use this when a slash command changes profile mid-session.
    pub fn set_permission_profile(&mut self, profile: Profile, allowlist: Vec<String>) {
        self.engine = PermissionEngine::new(profile, allowlist);
    }

    /// Set the reasoning effort for subsequent turns — switchable from the
    /// REPL, and overridable per harness step (high for planning, low for
    /// mechanical edits).
    pub fn set_reasoning_effort(&mut self, effort: Option<localpilot_llm::ReasoningEffort>) {
        self.config.reasoning_effort = effort;
    }

    /// The currently requested reasoning effort.
    #[must_use]
    pub fn reasoning_effort(&self) -> Option<localpilot_llm::ReasoningEffort> {
        self.config.reasoning_effort
    }

    /// A clonable handle for queueing steering input into a running turn.
    /// Queued text is admitted at the next safe provider-turn boundary.
    #[must_use]
    pub fn steer_queue(&self) -> SteerQueue {
        self.steer.clone()
    }

    /// The hook fabric, for registering observers, context hooks, and tool
    /// gates. Gates are tighten-only and run after the permission engine.
    pub fn hooks_mut(&mut self) -> &mut HookFabric {
        &mut self.hooks
    }

    /// Clear user/assistant/tool history while preserving the leading setup
    /// messages required for future turns.
    pub fn clear_conversation(&mut self) {
        let leading_system = self
            .messages
            .iter()
            .take_while(|message| message.role == Role::System)
            .filter(|message| !is_compaction_summary(message))
            .cloned()
            .collect();
        self.messages = leading_system;
        self.last_quota = None;
        self.history_generation += 1;
    }

    /// Compact the stored runtime message history using the same rules applied
    /// before automatic provider requests.
    #[must_use]
    pub fn compact_conversation(&mut self) -> ManualCompaction {
        let result = self.compacted_history();
        let context_used = estimate_tokens(&result.messages);
        self.messages = result.messages;
        self.history_generation += 1;
        ManualCompaction {
            compacted: result.compacted,
            context_used,
            context_limit: self.config.context_token_limit,
        }
    }

    /// Estimated context usage for the currently stored runtime history.
    #[must_use]
    pub fn context_usage(&self) -> (usize, usize) {
        (
            estimate_tokens(&self.messages),
            self.config.context_token_limit,
        )
    }

    /// Run the quality-gate checks whose cadence maps to `trigger`, through this
    /// session's own permission engine and approver — the same path tool calls
    /// take, so a check never bypasses a permission decision. Returns one outcome
    /// per matching check, in declaration order.
    pub async fn run_gate_checks(
        &self,
        checks: &[CheckConfig],
        trigger: Trigger,
        root: &Path,
    ) -> Vec<CheckOutcome> {
        let runner = CheckRunner::new(
            &self.engine,
            self.approver.as_ref(),
            self.config.interactivity,
            self.config.trusted,
            root,
        );
        let mut outcomes = Vec::new();
        for check in checks {
            if trigger_for_cadence(check.cadence) == trigger {
                let outcome = runner.run(check).await;
                self.hooks.notify(&HookEvent::GateCheck {
                    name: outcome.name.clone(),
                    passed: outcome.passed(),
                });
                outcomes.push(outcome);
            }
        }
        outcomes
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
                            let _ = events.send(RuntimeEvent::QuotaPaused {
                                reset: reset.clone(),
                            });
                            self.hooks.notify(&HookEvent::QuotaPaused {
                                reset: reset.clone(),
                            });
                            self.record_event(SessionEventKind::QuotaPaused { reset });
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
        self.record_event(SessionEventKind::Message {
            origin: origin_for(&message),
            message: message.clone(),
        });
        self.messages.push(message);
        self.history_generation += 1;
    }

    /// Compact the live history for the next request, reusing the cached
    /// result while the history is unchanged.
    fn compacted_history(&mut self) -> CompactionResult {
        if let Some((generation, cached)) = &self.compaction_cache {
            if *generation == self.history_generation {
                return cached.clone();
            }
        }
        let result = compact_with_summary(self.messages.clone(), self.config.context_token_limit);
        if result.compacted {
            if let Some(summary) = result.summary.clone() {
                self.record_event(SessionEventKind::Compacted { summary });
            }
            self.hooks.notify(&HookEvent::Compacted);
        }
        self.compaction_cache = Some((self.history_generation, result.clone()));
        result
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
        // Context hooks contribute system context for this turn through the
        // same seeded-system path a host would use.
        for context in self.hooks.context_for(user_input) {
            self.seed_system(context);
        }
        self.append(Message::text(Role::User, user_input));
        self.last_quota = None;
        let mut tool_calls_used = 0u32;
        let mut tools_enabled = true;

        for _ in 0..self.config.max_turns {
            if cancel.is_cancelled() {
                return self.stop(events, StopReason::Cancelled);
            }

            // Admit queued steering input at this safe boundary: after the
            // previous iteration's tool calls, before the next provider call.
            for steer_text in self.steer.drain() {
                self.append(Message::text(Role::User, steer_text));
            }

            let compacted = self.compacted_history();
            let used = estimate_tokens(&compacted.messages);
            let _ = events.send(RuntimeEvent::ContextUsage {
                used,
                limit: self.config.context_token_limit,
            });
            let tools = if tools_enabled {
                self.tool_specs()
            } else {
                Vec::new()
            };
            // Fold the compaction summary into the single leading system block
            // so providers never receive two consecutive system messages.
            let request_messages = crate::compaction::merge_consecutive_system(compacted.messages);
            let request = ModelRequest::new(self.config.model.clone(), request_messages)
                .with_tools(tools)
                .with_reasoning_effort(self.config.reasoning_effort);

            self.record_event(SessionEventKind::TurnStarted {
                model: self.config.model.clone(),
            });
            self.hooks.notify(&HookEvent::TurnStarted {
                model: self.config.model.clone(),
            });
            let mut stream = match self.open_stream(&request, events, cancel).await {
                Ok(stream) => stream,
                Err(StreamOpen::Cancelled) => return self.stop(events, StopReason::Cancelled),
                Err(StreamOpen::Failed) => return self.stop(events, StopReason::ProviderError),
            };

            let mut text = String::new();
            let mut reasoning = String::new();
            let mut calls: Vec<(String, String, serde_json::Value)> = Vec::new();
            let mut stream_failed = false;
            // Live degenerate-output guard, fed incrementally so a runaway
            // stream is aborted early without rescanning the whole turn.
            let mut monitor = StreamMonitor::default();

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
                            monitor.push(&delta);
                            if monitor.detected() {
                                let _ = events.send(RuntimeEvent::Warning(
                                    "degenerate output detected; stopping generation"
                                        .to_string(),
                                ));
                                break;
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
                            self.record_event(SessionEventKind::UsageReported {
                                input_tokens: usage.input_tokens,
                                output_tokens: usage.output_tokens,
                            });
                        }
                        Some(Ok(ModelEvent::ProviderWarning { message })) => {
                            let _ = events.send(RuntimeEvent::Warning(message));
                        }
                        Some(Ok(ModelEvent::Done)) => break,
                        Some(Ok(_)) => {}
                        Some(Err(err)) => {
                            self.last_quota = err.quota().cloned();
                            if let Some(reset) = self.last_quota.as_ref().map(quota_reset_label) {
                                let _ = events.send(RuntimeEvent::QuotaPaused {
                                    reset: reset.clone(),
                                });
                                self.hooks.notify(&HookEvent::QuotaPaused {
                                    reset: reset.clone(),
                                });
                                self.record_event(SessionEventKind::QuotaPaused { reset });
                            }
                            let _ = events
                                .send(RuntimeEvent::Warning(format!("stream error: {err}")));
                            if stream_error_stops_turn(&err) {
                                return self.stop(events, StopReason::ProviderError);
                            }
                            stream_failed = true;
                            break;
                        }
                        None => {
                            let _ = events.send(RuntimeEvent::Warning(
                                "stream ended before a completion marker".to_string(),
                            ));
                            stream_failed = true;
                            break;
                        },
                    }
                }
            }

            // Bad-output detection and recovery.
            let bad = if stream_failed {
                Some(localpilot_recovery::BadOutputKind::MalformedStructuredOutput)
            } else {
                detect(&text, !calls.is_empty())
            };
            if let Some(kind) = bad {
                let diagnostic = self.recovery.record_bad_turn(kind);
                self.persist_recovery(&diagnostic);
                let _ = events.send(RuntimeEvent::Recovery {
                    health: self.recovery.health(),
                });
                self.record_event(SessionEventKind::RecoveryDiagnostic {
                    kind: format!("{kind:?}"),
                    health: format!("{:?}", self.recovery.health()),
                });
                self.hooks.notify(&HookEvent::Recovery {
                    health: self.recovery.health(),
                });
                if self.recovery.health() == ModelHealth::Degraded {
                    return self.stop(events, StopReason::Degraded);
                }
                if matches!(
                    kind,
                    localpilot_recovery::BadOutputKind::SlashFlood
                        | localpilot_recovery::BadOutputKind::RepeatedTokenLoop
                ) && tools_enabled
                {
                    tools_enabled = false;
                    let _ = events.send(RuntimeEvent::Warning(
                        "retrying the degenerate response without tool schemas".to_string(),
                    ));
                }
                // Persisted and marked synthetic: the repair prompt shapes the
                // conversation the model sees, so a resumed session must
                // reconstruct it.
                self.append(
                    Message::text(Role::User, REPAIR_PROMPT).into_synthetic("repair prompt"),
                );
                continue;
            }
            self.recovery.record_clean_turn();

            // Validate the batch before persisting: a `tool_use` block with a
            // blank id can never be answered by a `tool_result`, so it must
            // not enter history at all. Every persisted `tool_use` is
            // guaranteed an answer on every exit path below.
            let rejection = invalid_tool_calls(&calls);
            let calls: Vec<(String, String, serde_json::Value)> = if rejection.is_some() {
                calls
                    .into_iter()
                    .filter(|(id, _, _)| !id.trim().is_empty())
                    .collect()
            } else {
                calls
            };

            // Assemble and persist the assistant message.
            let mut content = Vec::new();
            let reasoning = trim_leading_blank_lines(reasoning);
            let text = trim_leading_blank_lines(text);

            if !reasoning.trim().is_empty() {
                content.push(ContentBlock::Reasoning {
                    text: reasoning,
                    signature: None,
                    provider_metadata: None,
                });
            }
            if !text.trim().is_empty() {
                content.push(ContentBlock::text(text));
            }
            for (id, name, input) in &calls {
                content.push(ContentBlock::ToolUse(ToolCall::new(
                    ToolUseId::from(id.as_str()),
                    name.clone(),
                    input.clone(),
                )));
            }
            if !content.is_empty() {
                self.append(Message::new(Role::Assistant, content));
            }

            if let Some(reason) = rejection {
                let _ = events.send(RuntimeEvent::Warning(reason.clone()));
                // Answer every persisted tool_use so the wire contract holds,
                // carrying the rejection reason back to the model.
                for (id, _, _) in &calls {
                    self.append(tool_error_message(
                        id,
                        &format!("tool call rejected: {reason}"),
                    ));
                }
                if calls.is_empty() {
                    // Nothing answerable was persisted; correct via a plain
                    // user message instead.
                    self.append(
                        Message::text(Role::User, reason).into_synthetic("tool call rejected"),
                    );
                }
                continue;
            }

            if calls.is_empty() {
                return self.stop(events, StopReason::Done);
            }

            // Execute tool calls through the permission-gated registry.
            for index in 0..calls.len() {
                if tool_calls_used >= self.config.max_tool_calls {
                    // The remaining calls are already persisted as tool_use
                    // blocks; answer each before stopping so no request built
                    // from this history violates the pairing contract.
                    for (id, _, _) in &calls[index..] {
                        self.append(tool_error_message(
                            id,
                            "tool budget exhausted; the call was not executed",
                        ));
                    }
                    return self.stop(events, StopReason::MaxToolCalls);
                }
                tool_calls_used += 1;
                let (id, name, input) = &calls[index];

                // Surface the task plan to the UI as the model updates it.
                if name == "update_plan" {
                    if let Some(steps) = parse_plan(input) {
                        let _ = events.send(RuntimeEvent::Plan(steps));
                    }
                }

                let _ = events.send(RuntimeEvent::ToolStarted {
                    id: id.clone(),
                    name: name.clone(),
                });
                self.record_event(SessionEventKind::ToolStarted {
                    id: id.clone(),
                    name: name.clone(),
                });
                self.hooks.notify(&HookEvent::ToolStarted {
                    id: id.clone(),
                    name: name.clone(),
                });
                let call = ToolCall::new(ToolUseId::from(id.as_str()), name.clone(), input.clone());
                // Cancellation races the executing tool: an abort synthesizes
                // an error result (the pairing contract holds), and dropping
                // the dispatch future drops spawned children, which are
                // configured to die with it instead of waiting out their
                // timeout. The aborted execution stays in the event log.
                let result = {
                    let retention = StoreRetention(&self.store);
                    let ctx = ToolContext {
                        workspace: &self.workspace,
                        interactivity: self.config.interactivity,
                        trusted: self.config.trusted,
                        retention: Some(&retention),
                    };
                    let gates = self.hooks.gates();
                    tokio::select! {
                        () = cancel.cancelled() => None,
                        result = self.tools.dispatch_gated(
                            &call,
                            &ctx,
                            &self.engine,
                            self.approver.as_ref(),
                            &gates,
                        ) => Some(result),
                    }
                };
                let Some(result) = result else {
                    let aborted = localpilot_core::ToolResult::error(
                        ToolUseId::from(id.as_str()),
                        "cancelled by the user; execution aborted",
                    );
                    self.record_event(SessionEventKind::ToolFinished {
                        id: id.clone(),
                        name: name.clone(),
                        is_error: true,
                    });
                    self.hooks.notify(&HookEvent::ToolFinished {
                        id: id.clone(),
                        name: name.clone(),
                        is_error: true,
                    });
                    self.append(Message::new(
                        Role::Tool,
                        vec![ContentBlock::ToolResult(aborted)],
                    ));
                    return self.stop(events, StopReason::Cancelled);
                };
                let _ = events.send(RuntimeEvent::ToolFinished {
                    id: result.id.to_string(),
                    name: name.clone(),
                    is_error: result.is_error,
                    output: result.output.clone(),
                });
                self.record_event(SessionEventKind::ToolFinished {
                    id: result.id.to_string(),
                    name: name.clone(),
                    is_error: result.is_error,
                });
                self.hooks.notify(&HookEvent::ToolFinished {
                    id: result.id.to_string(),
                    name: name.clone(),
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

    fn stop(&mut self, events: &broadcast::Sender<RuntimeEvent>, reason: StopReason) -> StopReason {
        if reason == StopReason::Cancelled {
            self.record_event(SessionEventKind::Cancelled);
        }
        self.record_event(SessionEventKind::TurnEnded {
            stop: format!("{reason:?}"),
        });
        self.hooks.notify(&HookEvent::TurnEnded { reason });
        let _ = events.send(RuntimeEvent::Stopped(reason));
        reason
    }

    fn persist_recovery(&self, diagnostic: &localpilot_recovery::RecoveryDiagnostic) {
        if let Ok(json) = serde_json::to_string(diagnostic) {
            let key = format!("recovery-{}", self.session_id);
            // Stored as a tool-output-style snapshot; redaction is applied by the
            // store and again here for defense in depth.
            let _ = self.store.put_tool_output(&key, &redact(&json));
        }
    }
}

/// A synthesized error `tool_result` answering a persisted `tool_use` that was
/// never executed (rejected batch or exhausted tool budget), keeping the
/// tool-pairing contract intact on every exit path.
fn tool_error_message(id: &str, output: &str) -> Message {
    Message::new(
        Role::Tool,
        vec![ContentBlock::ToolResult(
            localpilot_core::ToolResult::error(ToolUseId::from(id), output),
        )],
    )
}

fn trim_leading_blank_lines(mut text: String) -> String {
    let trimmed = text.trim_start_matches(['\r', '\n']);
    if trimmed.len() != text.len() {
        text.drain(..text.len() - trimmed.len());
    }
    text
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

fn stream_error_stops_turn(err: &ProviderError) -> bool {
    !matches!(err, ProviderError::StreamDecode(_))
}

fn is_compaction_summary(message: &Message) -> bool {
    message.content.iter().any(|block| match block {
        ContentBlock::Text { text } => {
            text.starts_with("Conversation summary for trimmed history:")
        }
        _ => false,
    })
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

/// Adapts the session store as the spill target for oversized tool outputs.
struct StoreRetention<'a>(&'a Store);

impl localpilot_tools::OutputRetention for StoreRetention<'_> {
    fn retain(&self, id: &str, output: &str) -> Result<(), String> {
        self.0
            .put_tool_output(id, output)
            .map_err(|err| err.to_string())
    }

    fn fetch(&self, id: &str) -> Result<Option<String>, String> {
        self.0.get_tool_output(id).map_err(|err| err.to_string())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_limit_derives_from_a_known_window_with_a_reserve() {
        assert_eq!(effective_context_limit(Some(32_768), 24_000), 28_672);
        // The configured global limit is the fallback only.
        assert_eq!(effective_context_limit(None, 24_000), 24_000);
        // A tiny window never collapses below the reserve floor.
        assert_eq!(effective_context_limit(Some(1_024), 24_000), 4_096);
    }
}
