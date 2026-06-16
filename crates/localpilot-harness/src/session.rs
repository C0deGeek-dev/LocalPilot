//! The agent-mode session runtime: the conversational loop both operating modes
//! share. It streams provider events, routes tool calls through the permission
//! engine, persists the transcript, and supports cancellation, recovery
//! safeguards, and context compaction.

use std::collections::HashMap;
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

use crate::compaction::{
    apply_smart_digest, compact_plan, estimate_tokens, CompactionMetadata, CompactionMode,
    CompactionResult,
};
use crate::hooks::{HookEvent, HookFabric};
use crate::quality::{CheckOutcome, CheckRunner};
use crate::rules::{trigger_for_cadence, Trigger};
use crate::summarizer::{FallbackReason, ProviderSummarizer, Summarizer, SummarizerTuning};

/// Why a turn loop stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The model produced a final answer.
    Done,
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
    /// A tool has failed repeatedly (≥ 6 times in this turn). The safeguard
    /// stops issuing that tool and notifies the user.
    ToolStuck { name: String, count: u32 },
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualCompaction {
    /// Whether older messages were removed and summarized.
    pub compacted: bool,
    /// Estimated context usage after compaction.
    pub context_used: usize,
    /// Configured context limit used for the operation.
    pub context_limit: usize,
    /// Requested compaction mode.
    pub requested_mode: CompactionMode,
    /// Mode that produced the final projection.
    pub used_mode: CompactionMode,
    /// Deterministic fallback reason, if a smart attempt did not take effect.
    pub fallback_reason: Option<String>,
}

/// Tuning for a session.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub model: String,
    pub interactivity: Interactivity,
    pub trusted: bool,
    pub context_token_limit: usize,
    /// Requested reasoning effort for provider turns; mapped (or no-op
    /// clamped) per provider. Switchable mid-session.
    pub reasoning_effort: Option<localpilot_llm::ReasoningEffort>,
    /// How many times to retry a transient connection failure (network or
    /// 5xx) before giving up, with exponential backoff between attempts.
    pub max_stream_retries: u32,
    /// Runtime context compaction mode.
    pub compaction_mode: CompactionMode,
    /// Budgets and timeout for an optional model-backed smart summarizer.
    pub summarizer_tuning: SummarizerTuning,
    /// When set, a tool contract's `RequiresPriorRead` precondition is enforced:
    /// a destructive overwrite of an existing, unread file is refused before the
    /// permission engine. Off by default so existing flows are unchanged; the
    /// discipline track opts in to measure its false-positive rate before any
    /// default-on decision.
    pub enforce_prior_read: bool,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            model: "default".to_string(),
            interactivity: Interactivity::Interactive,
            trusted: true,
            context_token_limit: 24_000,
            reasoning_effort: None,
            max_stream_retries: 3,
            compaction_mode: CompactionMode::Deterministic,
            summarizer_tuning: SummarizerTuning::default(),
            enforce_prior_read: false,
        }
    }
}

/// Tokens held back from the model's context window for the response and
/// protocol overhead when deriving the session budget from a real window.
const CONTEXT_RESERVE_TOKENS: usize = 4_096;

/// Smallest target a forced compaction will aim for, so `/compact force` on a
/// tiny configured limit still leaves a usable conversation.
const FORCE_COMPACT_FLOOR: usize = 4_096;

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

/// Default threshold at which a tool is considered stuck and the safeguard
/// intervenes.
const DEFAULT_TOOL_FAILURE_THRESHOLD: u32 = 6;

/// Inject per-turn context-hook output into the request message list.
///
/// The block is placed immediately after a leading system prompt (so both fold
/// into the single top-level system message), then consecutive system messages
/// are merged. With no context this is just the merge. The input `messages` are
/// the compacted *stored* history; the returned list is request-only and is
/// never written back to history.
fn inject_turn_context(messages: Vec<Message>, context: Option<Message>) -> Vec<Message> {
    let combined = match context {
        None => messages,
        Some(context) => {
            let mut out = Vec::with_capacity(messages.len() + 1);
            let mut rest = messages.into_iter();
            match rest.next() {
                // Keep the system prompt first, then the retrieval context.
                Some(first) if first.role == Role::System => {
                    out.push(first);
                    out.push(context);
                }
                // No leading system prompt: the context leads.
                Some(first) => {
                    out.push(context);
                    out.push(first);
                }
                None => out.push(context),
            }
            out.extend(rest);
            out
        }
    };
    crate::compaction::merge_consecutive_system(combined)
}

/// Tracks per-tool failure counts within a single turn. Resets at every turn
/// boundary so that failures from previous turns don't accumulate.
#[derive(Debug, Default)]
struct ToolFailureGuard {
    /// Maps tool name → failure count for this turn.
    failures: HashMap<String, u32>,
}

impl ToolFailureGuard {
    /// Record a failure for `tool_name` and return the new count.
    fn record_failure(&mut self, tool_name: &str) -> u32 {
        let count = self.failures.entry(tool_name.to_string()).or_insert(0);
        *count += 1;
        *count
    }

    /// Reset counters for a successful (non-error) tool invocation.
    fn record_success(&mut self, tool_name: &str) {
        self.failures.remove(tool_name);
    }

    /// Whether `tool_name` has exceeded the threshold.
    #[allow(dead_code)] // kept for potential future use (e.g., pre-check before dispatch)
    fn is_stuck(&self, tool_name: &str, threshold: u32) -> bool {
        self.failures.get(tool_name).copied().unwrap_or(0) >= threshold
    }

    /// Reset all counters (call at the start of each turn).
    fn reset(&mut self) {
        self.failures.clear();
    }
}

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
    /// The compaction result for the current `history_generation` and reserve,
    /// so the per-iteration request shaping does not recompact unchanged history.
    /// The reserve is the per-turn context-hook budget held back from the limit.
    compaction_cache: Option<(u64, usize, CompactionResult)>,
    /// Steering input queued by the host while a turn runs.
    steer: SteerQueue,
    /// Registered lifecycle observers, context hooks, and tool gates.
    hooks: HookFabric,
    /// Per-tool failure counts within the current turn.
    tool_failure_guard: ToolFailureGuard,
    /// Optional injected smart summarizer. When unset and smart mode is active,
    /// a provider-backed summarizer is built on demand from `provider`.
    summarizer: Option<Arc<dyn Summarizer>>,
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
            tool_failure_guard: ToolFailureGuard::default(),
            summarizer: None,
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

    /// Evaluate a tool's contract preconditions before it runs. Returns a
    /// model-visible block reason when a precondition is unmet (tighten-only:
    /// this can only refuse a call, never grant one). Projects the evidence
    /// ledger from this session's own event log.
    fn precondition_block(&self, name: &str, input: &serde_json::Value) -> Option<String> {
        if !self.config.enforce_prior_read {
            return None;
        }
        let contract = self.tools.get(name)?.contract();
        if contract.preconditions.is_empty() {
            return None;
        }
        let events = self.store.read_events(self.session_id).ok()?;
        let ledger = crate::evidence::EvidenceLedger::project(&events);
        crate::precondition::evaluate(contract.preconditions, input, &ledger, &self.workspace).err()
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

    /// Inject a smart summarizer backend. When set and smart mode is active, it
    /// is preferred over the provider-backed summarizer built on demand. Mainly
    /// for tests and hosts that want a dedicated summarization model.
    pub fn set_summarizer(&mut self, summarizer: Arc<dyn Summarizer>) {
        self.summarizer = Some(summarizer);
    }

    /// The active summarizer for smart-mode compaction, if any: the injected one
    /// when present, otherwise a provider-backed summarizer over the session's
    /// model. `None` when smart mode is not configured.
    fn active_summarizer(&self) -> Option<Arc<dyn Summarizer>> {
        if self.config.compaction_mode != CompactionMode::SmartWithFallback {
            return None;
        }
        if let Some(summarizer) = &self.summarizer {
            return Some(Arc::clone(summarizer));
        }
        Some(Arc::new(ProviderSummarizer::new(
            Arc::clone(&self.provider),
            self.config.model.clone(),
        )))
    }

    /// Compact the stored runtime message history using the same rules applied
    /// before automatic provider requests.
    pub async fn compact_conversation(&mut self) -> ManualCompaction {
        let cancel = CancellationToken::new();
        // Manual compaction shapes stored history only; no per-turn request
        // context is injected here, so nothing is reserved.
        let result = self.compacted_history(0, &cancel).await;
        let context_used = estimate_tokens(&result.messages);
        let fallback_reason = result.metadata.fallback_reason.clone();
        let (requested_mode, used_mode) =
            (result.metadata.requested_mode, result.metadata.used_mode);
        self.messages = result.messages;
        self.history_generation += 1;
        ManualCompaction {
            compacted: result.compacted,
            context_used,
            context_limit: self.config.context_token_limit,
            requested_mode,
            used_mode,
            fallback_reason,
        }
    }

    /// Force-compact the runtime history even when it is already within the
    /// configured limit, by targeting roughly half the budget. The token
    /// estimate undercounts some payloads (large tool outputs in particular), so
    /// a model's real tokenizer can reject a request the budget believes fits;
    /// this lets the user shrink the conversation on demand and keep going.
    pub async fn compact_conversation_force(&mut self) -> ManualCompaction {
        let target = (self.config.context_token_limit / 2).max(FORCE_COMPACT_FLOOR);
        let cancel = CancellationToken::new();
        let result = self.compact_candidate(target, &cancel).await;
        if result.compacted {
            if let Some(summary) = result.summary.clone() {
                self.record_event(SessionEventKind::Compacted { summary });
            }
            self.record_compaction_attempt("completed", &result.metadata);
            self.hooks.notify(&HookEvent::Compacted);
        }
        let context_used = estimate_tokens(&result.messages);
        let fallback_reason = result.metadata.fallback_reason.clone();
        let (requested_mode, used_mode) =
            (result.metadata.requested_mode, result.metadata.used_mode);
        self.messages = result.messages;
        self.history_generation += 1;
        self.compaction_cache = None;
        ManualCompaction {
            compacted: result.compacted,
            context_used,
            context_limit: self.config.context_token_limit,
            requested_mode,
            used_mode,
            fallback_reason,
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

    /// Seed a system message into the conversation — for example durable host
    /// context injected before a turn. Persisted and counted in context like any
    /// message. Per-turn retrieval that is re-derived every turn is *not* seeded
    /// here; context hooks contribute it and it is injected into the request at
    /// build time (see `run_turn`), so it never accumulates in history.
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
                        // A context-length rejection at open time is a missed
                        // local estimate; the turn loop shrinks and retries once.
                        if let ProviderError::InvalidRequest { .. } = err {
                            return Err(StreamOpen::Overflow);
                        }
                        return Err(StreamOpen::Failed);
                    }
                }
            }
        }
    }

    /// Handle a provider overflow: on the first occurrence this turn, compact
    /// active history tighter and report that a retry will follow (returns
    /// `true`); on a second overflow it reports a terminal failure (`false`).
    async fn try_overflow_retry(
        &mut self,
        retried: &mut bool,
        events: &broadcast::Sender<RuntimeEvent>,
        cancel: &CancellationToken,
    ) -> bool {
        if *retried {
            let _ = events.send(RuntimeEvent::Warning(
                "provider rejected the request as too large again; stopping the turn".to_string(),
            ));
            return false;
        }
        *retried = true;
        let _ = events.send(RuntimeEvent::Warning(
            "provider rejected the request as too large; compacting and retrying once".to_string(),
        ));
        self.shrink_for_overflow(cancel).await;
        true
    }

    /// Compact active history to roughly half its current estimate (no smaller
    /// than the force floor) so a retry fits, recording the attempt as an
    /// overflow-driven compaction in the audit log.
    async fn shrink_for_overflow(&mut self, cancel: &CancellationToken) {
        let target = (estimate_tokens(&self.messages) / 2).max(FORCE_COMPACT_FLOOR);
        let result = self.compact_candidate(target, cancel).await;
        if result.compacted {
            if let Some(summary) = result.summary.clone() {
                self.record_event(SessionEventKind::Compacted { summary });
            }
            self.record_compaction_attempt("overflow_retry", &result.metadata);
            self.hooks.notify(&HookEvent::Compacted);
            self.messages = result.messages;
            self.history_generation += 1;
            self.compaction_cache = None;
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
    /// result while the history and the reserve are unchanged. `reserve` is the
    /// token budget held back for context that will be injected into the request
    /// but is not part of the stored history (per-turn context-hook retrieval),
    /// so the compacted history plus that context still fits the limit.
    async fn compacted_history(
        &mut self,
        reserve: usize,
        cancel: &CancellationToken,
    ) -> CompactionResult {
        if let Some((generation, cached_reserve, cached)) = &self.compaction_cache {
            if *generation == self.history_generation && *cached_reserve == reserve {
                return cached.clone();
            }
        }
        let limit = self.config.context_token_limit.saturating_sub(reserve);
        let result = self.compact_candidate(limit, cancel).await;
        if result.compacted {
            if let Some(summary) = result.summary.clone() {
                self.record_event(SessionEventKind::Compacted { summary });
            }
            self.record_compaction_attempt("completed", &result.metadata);
            self.hooks.notify(&HookEvent::Compacted);
        }
        self.compaction_cache = Some((self.history_generation, reserve, result.clone()));
        result
    }

    /// Build the deterministic projection, then — in smart mode — make one
    /// bounded summarization attempt and adopt it only on completed-only
    /// cutover. Any smart failure leaves the deterministic projection in force
    /// with a typed fallback reason recorded for audit.
    async fn compact_candidate(
        &self,
        token_limit: usize,
        cancel: &CancellationToken,
    ) -> CompactionResult {
        let plan = compact_plan(self.messages.clone(), token_limit);
        let mut result = plan.result;
        result.metadata.requested_mode = self.config.compaction_mode;
        if !result.compacted {
            return result;
        }
        let Some(summarizer) = self.active_summarizer() else {
            return result;
        };
        if plan.dropped.is_empty() {
            result.metadata.used_mode = CompactionMode::Deterministic;
            result.metadata.fallback_reason =
                Some(FallbackReason::NothingToSummarize.as_str().to_string());
            return result;
        }
        match summarizer
            .summarize(
                &plan.dropped,
                &plan.carried,
                self.config.summarizer_tuning,
                cancel,
            )
            .await
        {
            Ok(smart) => apply_smart_digest(result, &plan.dropped, smart, token_limit),
            Err(reason) => {
                result.metadata.used_mode = CompactionMode::Deterministic;
                result.metadata.fallback_reason = Some(reason.as_str().to_string());
                result
            }
        }
    }

    fn record_compaction_attempt(&mut self, state: &str, metadata: &CompactionMetadata) {
        self.record_event(SessionEventKind::CompactionAttempt {
            requested_mode: compaction_mode_label(metadata.requested_mode).to_string(),
            used_mode: compaction_mode_label(metadata.used_mode).to_string(),
            state: state.to_string(),
            dropped_exchanges: metadata.dropped_exchanges,
            kept_messages: metadata.kept_messages,
            dropped_messages: metadata.dropped_messages,
            digest_estimate_tokens: metadata.digest_estimate_tokens,
            fallback_reason: metadata.fallback_reason.clone(),
            truncated_tool_results: metadata.truncated_tool_results,
        });
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
        // Context hooks contribute system context for this turn. It is computed
        // once from the prompt and injected into the outgoing request adjacent to
        // the leading system prompt — never appended to history or persisted — so
        // re-derived retrieval cannot accumulate, the transcript stays equal to
        // the authored history, and the block folds into the top-level system
        // rather than riding the wire as a resent user message. Its token cost is
        // reserved from the compaction budget so the request still fits the limit.
        let retrieval_text = self.hooks.context_for(user_input).join("\n");
        let turn_context = (!retrieval_text.is_empty())
            .then(|| Message::new(Role::System, vec![ContentBlock::text(retrieval_text)]));
        let context_reserve = turn_context
            .as_ref()
            .map_or(0, |message| estimate_tokens(std::slice::from_ref(message)));
        self.append(Message::text(Role::User, user_input));
        self.last_quota = None;
        self.tool_failure_guard.reset();
        let mut tools_enabled = true;
        // A provider may reject a request as too large even when the local
        // estimate believed it fit. The first overflow forces a tighter
        // compaction and one retry; a second overflow this turn is terminal.
        let mut overflow_retried = false;

        loop {
            if cancel.is_cancelled() {
                return self.stop(events, StopReason::Cancelled);
            }

            // Admit queued steering input at this safe boundary: after the
            // previous iteration's tool calls, before the next provider call.
            for steer_text in self.steer.drain() {
                self.append(Message::text(Role::User, steer_text));
            }

            let compacted = self.compacted_history(context_reserve, cancel).await;
            let tools = if tools_enabled {
                self.tool_specs()
            } else {
                Vec::new()
            };
            // Inject the per-turn retrieval context after the leading system
            // prompt, then fold consecutive system blocks so the provider sees a
            // single leading system message (and never two in a row). The token
            // usage reported is the real request total, including injected context.
            let request_messages = inject_turn_context(compacted.messages, turn_context.clone());
            let used = estimate_tokens(&request_messages);
            let _ = events.send(RuntimeEvent::ContextUsage {
                used,
                limit: self.config.context_token_limit,
            });
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
                Err(StreamOpen::Overflow) => {
                    if self
                        .try_overflow_retry(&mut overflow_retried, events, cancel)
                        .await
                    {
                        continue;
                    }
                    return self.stop(events, StopReason::ProviderError);
                }
            };

            let mut text = String::new();
            let mut reasoning = String::new();
            let mut calls: Vec<(String, String, serde_json::Value)> = Vec::new();
            let mut stream_failed = false;
            let mut output_limited = false;
            let mut overflow = false;
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
                        Some(Ok(ModelEvent::OutputLimit { message })) => {
                            output_limited = true;
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
                            // A context-length rejection mid-stream is an
                            // overflow, not a fatal turn error: shrink and retry
                            // once before giving up.
                            if matches!(err, ProviderError::InvalidRequest { .. }) {
                                overflow = true;
                                break;
                            }
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

            if overflow {
                // Completed-only: the failed request streamed nothing durable;
                // shrink active history and retry once, else stop.
                if self
                    .try_overflow_retry(&mut overflow_retried, events, cancel)
                    .await
                {
                    continue;
                }
                return self.stop(events, StopReason::ProviderError);
            }

            if output_limited {
                let message = "discarding partial response because the provider hit the output token limit; increase provider max_tokens or ask for a shorter answer".to_string();
                let _ = events.send(RuntimeEvent::Warning(message));
                return self.stop(events, StopReason::ProviderError);
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
            let reasoning = trim_blank_boundary_lines(reasoning);
            let text = trim_blank_boundary_lines(text);

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

            if let Some(message) = invalid_tool_calls(&calls) {
                let _ = events.send(RuntimeEvent::Warning(message.clone()));
                let diagnostic = self
                    .recovery
                    .record_bad_turn(localpilot_recovery::BadOutputKind::MalformedToolCall);
                self.persist_recovery(&diagnostic);
                let _ = events.send(RuntimeEvent::Recovery {
                    health: self.recovery.health(),
                });
                if self.recovery.health() == ModelHealth::Degraded {
                    return self.stop(events, StopReason::Degraded);
                }
                self.messages.push(Message::text(Role::User, message));
                continue;
            }

            // Execute tool calls through the permission-gated registry.
            for (id, name, input) in &calls {
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
                let result = if let Some(reason) = self.precondition_block(name, input) {
                    // A contract precondition (e.g. RequiresPriorRead) refused the
                    // call before permission. Tighten-only: surface it as an error
                    // result so the model sees why and can recover.
                    Some(localpilot_core::ToolResult::error(
                        ToolUseId::from(id.as_str()),
                        reason,
                    ))
                } else {
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

                // Track per-tool failure counts for the safeguard.
                if result.is_error {
                    let count = self.tool_failure_guard.record_failure(name);
                    match count.cmp(&DEFAULT_TOOL_FAILURE_THRESHOLD) {
                        std::cmp::Ordering::Less => {
                            let _ = events.send(RuntimeEvent::Warning(format!(
                                "tool `{name}` failed ({}/{})",
                                count, DEFAULT_TOOL_FAILURE_THRESHOLD
                            )));
                        }
                        std::cmp::Ordering::Equal => {
                            let msg = format!(
                                "tool `{name}` has failed {count} times this turn; stopping further \
                                 calls and trying another approach"
                            );
                            let _ = events.send(RuntimeEvent::Warning(msg.clone()));
                            let _ = events.send(RuntimeEvent::ToolStuck {
                                name: name.clone(),
                                count,
                            });
                        }
                        std::cmp::Ordering::Greater => {
                            let _ = events.send(RuntimeEvent::Warning(format!(
                                "tool `{name}` failed again (#{count}); still stuck"
                            )));
                        }
                    }
                } else {
                    self.tool_failure_guard.record_success(name);
                }
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
/// never executed (a rejected batch), keeping the tool-pairing contract intact
/// on every exit path.
fn tool_error_message(id: &str, output: &str) -> Message {
    Message::new(
        Role::Tool,
        vec![ContentBlock::ToolResult(
            localpilot_core::ToolResult::error(ToolUseId::from(id), output),
        )],
    )
}

fn trim_blank_boundary_lines(mut text: String) -> String {
    let trimmed = text.trim_matches(['\r', '\n']);
    if trimmed.len() != text.len() {
        text = trimmed.to_string();
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

fn compaction_mode_label(mode: CompactionMode) -> &'static str {
    match mode {
        CompactionMode::Deterministic => "deterministic",
        CompactionMode::SmartWithFallback => "smart_with_fallback",
    }
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
    /// The provider rejected the request as too large (a missed local estimate);
    /// the turn loop shrinks active history and retries once.
    Overflow,
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

    #[test]
    fn assistant_text_trims_blank_boundary_lines() {
        assert_eq!(
            trim_blank_boundary_lines("\r\n\nThe answer\n\n".to_string()),
            "The answer"
        );
    }

    #[test]
    fn tool_failure_guard_tracks_failures_per_tool() {
        let mut guard = ToolFailureGuard::default();
        assert_eq!(guard.record_failure("read_file"), 1);
        assert_eq!(guard.record_failure("read_file"), 2);
        assert_eq!(guard.record_failure("write_file"), 1);
    }

    #[test]
    fn tool_failure_guard_reaches_threshold_at_six() {
        let mut guard = ToolFailureGuard::default();
        for i in 1..=5 {
            assert_eq!(guard.record_failure("run_shell"), i);
        }
        // Sixth failure crosses the threshold.
        assert_eq!(guard.record_failure("run_shell"), 6);
    }

    #[test]
    fn tool_failure_guard_clears_on_success() {
        let mut guard = ToolFailureGuard::default();
        guard.record_failure("edit_file");
        guard.record_failure("edit_file");
        guard.record_success("edit_file");
        // After success the counter is gone.
        assert!(!guard.is_stuck("edit_file", 6));
    }

    #[test]
    fn tool_failure_guard_resets_across_turns() {
        let mut guard = ToolFailureGuard::default();
        for _ in 0..6 {
            guard.record_failure("find_files");
        }
        assert!(guard.is_stuck("find_files", 6));

        // Simulate a new turn boundary.
        guard.reset();
        assert!(!guard.is_stuck("find_files", 6));
    }

    #[test]
    fn tool_failure_guard_independent_per_tool() {
        let mut guard = ToolFailureGuard::default();
        for _ in 0..5 {
            guard.record_failure("tool_a");
        }
        // tool_a is at 5, not yet stuck.
        assert!(!guard.is_stuck("tool_a", 6));
        // tool_b hasn't failed at all.
        assert!(!guard.is_stuck("tool_b", 6));

        guard.record_failure("tool_b");
        assert_eq!(guard.record_failure("tool_b"), 2);
    }
}
