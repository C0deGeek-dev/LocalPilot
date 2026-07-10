//! The agent-mode session runtime: the conversational loop both operating modes
//! share. It streams provider events, routes tool calls through the permission
//! engine, persists the transcript, and supports cancellation, recovery
//! safeguards, and context compaction.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use indexmap::IndexMap;
use localpilot_config::redact::redact;
use localpilot_config::{CheckConfig, RuleSeverity};
use localpilot_core::{
    ContentBlock, EventId, Message, Role, SessionId, TokenUsage, ToolCall, ToolResult, ToolUseId,
};
use localpilot_llm::{
    InputBlockKind, ModelEvent, ModelEventStream, ModelProvider, ModelRequest, ProviderError,
    ProviderRegistry, QuotaInfo, ToolSpec,
};
use localpilot_recovery::{
    detect, BudgetController, BudgetDecision, ModelHealth, NoProgressDetector, RecoveryAction,
    RecoveryEngine, RepeatedErrorBreaker, StreamMonitor,
};
use localpilot_sandbox::{
    Approver, Interactivity, PermissionEngine, PermissionEngineHandle, Profile,
};
use localpilot_store::{origin_for, transcript_from_events, OpenReason, SessionEventKind, Store};
use localpilot_tools::{Broker, ToolContext, ToolRegistry};
use localpilot_verify::{DeterministicVerifier, Observation, Verdict, VerificationInput, Verifier};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::compaction::{
    apply_smart_digest, compact_plan, estimate_tokens, CompactionMetadata, CompactionMode,
    CompactionResult,
};
use crate::dispatch_gate::{pre_dispatch_decision, PreDispatch};
use crate::hooks::{HookEvent, HookFabric};
use crate::launch_targets::{self, LocalTarget};
use crate::quality::{CheckOutcome, CheckRunner, CheckStatus};
use crate::rules::{trigger_for_cadence, RuleContext, RuleEngine, RuleVerdict, Trigger};
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
    /// The per-turn tool-call budget was exhausted; the loop stopped to bound
    /// cost. Distinct from the attempt limit — that bounds *retries*, this
    /// bounds total tool calls in a turn. This is the hard cost-contract ceiling.
    BudgetExceeded,
    /// The turn was making no forward progress — the same successful calls
    /// repeating, or a tiny cycle of calls — and had reached the soft start, so
    /// the loop stopped rather than spend the rest of the budget spinning.
    /// Distinct from `BudgetExceeded`, which is the absolute cost ceiling.
    NoProgress,
    /// The turn exceeded its bounded wall-clock timeout (`turn_timeout`) and was
    /// stopped so a non-interactive caller gets a terminal state instead of an
    /// unbounded hang. The per-turn handoff (`last_turn_handoff`) summarizes what
    /// the turn had done when it was cut off.
    TimedOut,
}

/// A bounded, parseable summary of what a turn accomplished, surfaced at the
/// turn's single exit so a non-interactive caller always has a terminal state to
/// read — even when the turn timed out or was cut off mid-flight. It is derived
/// from per-turn state the runtime already tracks; the granular durable record
/// stays the session event log (`ToolFinished`/`MemoriesUsed`/`TurnEnded`), so
/// this adds no second reporting channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnHandoff {
    /// Why the turn stopped.
    pub reason: StopReason,
    /// How many tool calls the turn executed.
    pub tool_calls: usize,
    /// Workspace files the turn wrote, edited, or deleted (best-effort, by the
    /// path argument of each successful file-write tool call).
    pub files_changed: Vec<String>,
    /// Whether the turn persisted any learning to memory. Always `false` on the
    /// `print` one-shot path, which reads accepted memory but never closes out —
    /// the field surfaces that so a caller knows to run an explicit close-out.
    pub memory_written: bool,
}

impl TurnHandoff {
    /// Render the handoff as one machine-readable JSON line (no trailing newline),
    /// for a non-interactive caller to parse off the diagnostics stream.
    #[must_use]
    pub fn to_json_line(&self) -> String {
        let files = self
            .files_changed
            .iter()
            .map(|f| serde_json::Value::String(f.clone()))
            .collect::<Vec<_>>();
        serde_json::json!({
            "stop": format!("{:?}", self.reason),
            "tool_calls": self.tool_calls,
            "files_changed": files,
            "memory_written": self.memory_written,
        })
        .to_string()
    }
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
    /// 5xx) before giving up, with exponential backoff between attempts. Also
    /// bounds retries of a mid-stream truncation (the server dropped the
    /// response before completing it — see [`ProviderError::StreamTruncated`]).
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
    /// Soft start for the per-turn tool-call ceiling. A turn that keeps making
    /// progress runs past this up to `tool_call_budget_max`; a turn detected as
    /// making no forward progress stops here. An ordinary task stays well under
    /// it. Distinct from the attempt limit (which bounds retries). `None`
    /// disables the budget — the default, so a turn runs unbounded unless an
    /// operator opts in.
    pub tool_call_budget: Option<usize>,
    /// Hard cost-contract ceiling: the per-turn tool-call count that always
    /// stops the loop regardless of progress, so a turn can never run unbounded.
    /// With `tool_call_budget_max == tool_call_budget` the ceiling is the flat
    /// fixed budget; raising it lets a productive turn extend past the soft start.
    /// `None` disables the ceiling; setting either budget field enables the budget.
    pub tool_call_budget_max: Option<usize>,
    /// Whether the budget came from an explicit operator `[harness]` value rather
    /// than the built-in default fill (ADR-0055). It governs *who owns the
    /// no-progress stop*: with an explicit budget the cost controller owns it
    /// (defer the always-on guard), but the built-in default must keep the
    /// always-on degenerate-loop guard (ADR-0052: repeated/cyclic calls or a run
    /// of consecutive failures) active — otherwise a built-in `..._max` with no
    /// soft start collapses to `soft == hard`, which disables both the controller's
    /// no-progress branch and (when keyed on `hard_max`) the always-on guard,
    /// leaving a stuck turn to burn the full ceiling. `false` (this programmatic
    /// default) keeps the always-on guard, matching a library `SessionConfig` that
    /// bypasses `resolved_rails`.
    pub tool_budget_explicit: bool,
    /// When set, the no-unsupported-claim gate reviews the final reply: an
    /// action-completion claim that no `Verified` call supports is flagged.
    /// Off by default until the benchmark shows a low false-positive rate.
    pub enforce_claim_gate: bool,
    /// Per-rule severity overrides (`[harness.rules]`) consulted by the
    /// session-level rule engine — currently the `check_before_launch` discipline
    /// rule. Empty leaves every rule at its own default.
    pub rules: IndexMap<String, RuleSeverity>,
    /// When set (and the pull-discovery broker is installed), the harness parses
    /// assistant output for a `NEED: <capability>` marker and reveals the closest
    /// tool proactively. Off by default — the marker needs new model behaviour, so
    /// it ships opt-in (ADR-0031); failure-driven re-resolution carries the feature
    /// without it.
    pub tool_marker_enabled: bool,
    /// Bounded per-turn wall-clock timeout. When set, a turn that runs longer is
    /// stopped with [`StopReason::TimedOut`] and a parseable handoff instead of
    /// hanging — the bound a non-interactive caller relies on. `None` (the
    /// default) leaves a turn unbounded, so existing flows are unchanged.
    pub turn_timeout: Option<std::time::Duration>,
    /// When set, a tool call whose arguments do not match the tool schema is
    /// answered with a concise, schema-aware error (built from the schema and the
    /// tool's curated examples) instead of the raw deserializer string, so the
    /// model can self-correct on the next turn. Off in this programmatic default
    /// (so existing flows see the raw message); the production config maps
    /// `[tools] readable_errors`, which defaults on. The raw detail is always
    /// retained in the logs/telemetry.
    pub enforce_readable_errors: bool,
    /// Conservative, schema-guided repair of a shape-invalid tool call's arguments
    /// (`off|warn|on`). `off` (this programmatic default) never rewrites arguments;
    /// the production config maps `[tools] repair`. Repair never touches a
    /// destructive/external/MCP tool or a content/command field, and a repaired
    /// call carries a model-visible note.
    pub repair_mode: localpilot_config::RepairMode,
    /// When set, a turn that would finalize with no tool call first runs a
    /// workspace verification command (build/test); on failure the diagnostics
    /// are fed back and the loop continues instead of declaring success on code
    /// that never compiled. Off by default (an opt-in feature lever); the
    /// production config maps `[harness] verify_before_done`. Bounded so it can
    /// never loop forever: the budget/timeout rails plus a fixed re-entry cap.
    pub verify_before_done: bool,
    /// Override command for the verify-before-done gate (a single command line,
    /// split on whitespace — no shell). `None` resolves the command from the
    /// workspace stack. Maps `[harness] verify_command`.
    pub verify_command: Option<String>,
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
            tool_call_budget: None,
            tool_call_budget_max: None,
            tool_budget_explicit: false,
            enforce_claim_gate: false,
            rules: IndexMap::new(),
            tool_marker_enabled: false,
            turn_timeout: None,
            enforce_readable_errors: false,
            repair_mode: localpilot_config::RepairMode::Off,
            verify_before_done: false,
            verify_command: None,
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

/// Repair prompt for a malformed *file-write* tool call: the model could not
/// emit the whole write as one well-formed call (typically too large). Steer it
/// to write the file in pieces instead of replaying the same oversized call.
const CHUNKED_WRITE_REPAIR_PROMPT: &str =
    "Your previous file-write tool call was too large to parse as one call. Write the file in \
     smaller pieces: create it with `write_file` containing the first section, then add each \
     remaining section with `append_file`. Keep every individual call small.";

/// Whether a tool name is one of the file-write builtins a chunked-write
/// instruction applies to.
fn is_file_write_tool(name: &str) -> bool {
    matches!(
        name,
        "write_file"
            | "append_file"
            | "edit_file"
            | "multi_edit"
            | "replace_in_file"
            | "apply_patch"
    )
}

/// Best-effort target path of a file-write tool call, read from its `path`
/// argument (the key every file-write builtin uses). `None` when the call carries
/// no string path (e.g. a patch body), so the handoff simply omits it.
fn file_write_path(input: &serde_json::Value) -> Option<String> {
    input
        .get("path")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

/// Default threshold at which a tool is considered stuck and the safeguard
/// intervenes.
const DEFAULT_TOOL_FAILURE_THRESHOLD: u32 = 6;

/// Always-on degenerate-loop guard (independent of the opt-in tool-call budget):
/// the number of consecutive failing tool calls, with no successful call in
/// between, that ends a turn even when the budget is off. A short error-recovery
/// streak is normal; this many failures in a row with nothing landing is a spin.
/// See ADR-0052.
const UNPRODUCTIVE_CALL_LIMIT: usize = 12;

/// Maximum times the verify-before-done gate may fail and re-enter the loop in a
/// single turn before the turn finalizes anyway. A conservative, fixed safety
/// cap so the gate can never loop forever on its own — independent of the
/// budget/timeout rails, which also bound it. After this many failed
/// verifications the turn ends `Done` with the failing state recorded.
const VERIFY_GATE_MAX_ATTEMPTS: usize = 3;

/// How the verify-before-done gate ends (or extends) a turn that would finalize.
enum VerifyGate {
    /// Finalize the turn as `Done` (gate off, no target, passed, or unrunnable).
    Finalize,
    /// Verification failed; feed this diagnostics text back and keep going.
    Retry(String),
    /// The re-entry cap was reached with the build still failing; stop the turn
    /// with `NoProgress`.
    GiveUp,
}

/// Stable store key under which the broker's graduated tools persist across
/// sessions (local and disposable, ADR-0012). Keyed by no session id so it is
/// shared by the project's sessions.
const GRADUATION_KEY: &str = "tool-graduation";

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

/// Extract `NEED: <capability>` markers from assistant output (ADR-0031). One per
/// line that begins — after trimming any leading list/bold punctuation — with a
/// case-insensitive `need:` prefix; the capability is the non-empty remainder.
/// Bounded so a runaway response cannot enqueue unbounded reveals.
fn parse_need_markers(text: &str) -> Vec<String> {
    const MAX_MARKERS: usize = 3;
    let mut needs = Vec::new();
    for line in text.lines() {
        let line = line.trim().trim_start_matches(['-', '*', ' ']);
        if line
            .get(..5)
            .is_some_and(|p| p.eq_ignore_ascii_case("need:"))
        {
            let need = line[5..].trim();
            if !need.is_empty() {
                needs.push(need.to_string());
                if needs.len() >= MAX_MARKERS {
                    break;
                }
            }
        }
    }
    needs
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

    /// Reset all counters (call at the start of each turn).
    fn reset(&mut self) {
        self.failures.clear();
    }
}

/// A strategy-change hint appended to a tool result when the same error has
/// repeated, so a weak model breaks the loop instead of re-sending the failing
/// call. First-party text; mirrors the system prompt's shell discipline.
fn same_error_hint(tool: &str) -> String {
    format!(
        "\n\n[recovery] `{tool}` has now failed the same way several times. Do not re-send the \
         same call — change approach: for a multiline or heavily-quoted shell command, write the \
         body to a script file (.py/.ps1/.sh) and run that file; if a required tool is missing, say \
         so instead of working around it; otherwise read the relevant file or inputs before \
         retrying."
    )
}

/// A strategy-change hint appended to a tool result when the turn is making no
/// forward progress — the same successful calls repeating, or a tiny cycle of
/// calls — so the model breaks out before the budget controller stops the turn.
/// First-party text; mirrors [`same_error_hint`].
fn no_progress_hint() -> String {
    "\n\n[recovery] These tool calls are not making forward progress — the same \
     calls keep returning the same results. Do not repeat them: either act on \
     what you already have and answer, or change approach (read a different \
     input, try a different tool, or state what is blocking you)."
        .to_string()
}

/// The result of re-pointing a live session at a different provider/model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchOutcome {
    /// The provider id now active.
    pub provider_id: String,
    /// The model now active.
    pub model: String,
    /// A non-fatal note for the user (e.g. the new provider had no configured
    /// default model, so the prior model name was kept).
    pub warning: Option<String>,
}

/// Why a mid-session provider/model switch was refused. The switch is a pure
/// runtime re-point, so the only failures are an in-flight turn or an id that is
/// not in the registry — never a network or auth error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SwitchError {
    /// A turn is in flight; the switch is refused until the next turn boundary so
    /// the transcript is never re-pointed mid-turn.
    #[error("a turn is in progress; switch again once it finishes")]
    TurnInFlight,
    /// No provider with this id is configured (or no registry was attached).
    #[error("provider '{0}' is not configured")]
    UnknownProvider(String),
}

/// The agent-mode runtime.
pub struct SessionRuntime {
    provider: Arc<dyn ModelProvider>,
    tools: ToolRegistry,
    /// Shared + swappable so an interactive host can change the permission
    /// profile while a turn is in flight; every tool call snapshots it fresh.
    engine: PermissionEngineHandle,
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
    /// Detects a tool failing repeatedly with the *same* error within the turn,
    /// so the model is nudged to change approach before the failure budget is
    /// spent. Reset each turn alongside `tool_failure_guard`.
    error_breaker: RepeatedErrorBreaker,
    /// Optional injected smart summarizer. When unset and smart mode is active,
    /// a provider-backed summarizer is built on demand from `provider`.
    summarizer: Option<Arc<dyn Summarizer>>,
    /// The deterministic rule engine, consulted at the tool-dispatch gate for the
    /// `check_before_launch` discipline rule.
    rule_engine: RuleEngine,
    /// Local serveable targets named in the task prompt(s) this session, against
    /// which a launch/scaffold action is checked for a prior probe.
    named_targets: Vec<LocalTarget>,
    /// The pull-discovery broker (ADR-0031), when enabled. `None` advertises the
    /// full registry every turn (today's behaviour, the rollback path). When set,
    /// the per-turn tool specs are narrowed to the broker's working set.
    broker: Option<Broker>,
    /// Long-running processes started by `run_background` this session. In-memory
    /// and session-scoped: every child is killed when the session closes (or this
    /// runtime drops), so no background server outlives the session.
    background: Arc<localpilot_tools::BackgroundProcesses>,
    /// The already-built provider registry, when the host attaches one. Present in
    /// interactive sessions so the active provider/model can be re-pointed
    /// mid-conversation (the switch is a lookup here, never a rebuild). `None`
    /// leaves the session single-provider — a switch is then refused as unknown.
    registry: Option<Arc<ProviderRegistry>>,
    /// A best-effort, probe-resolved image-input capability for the active
    /// provider, set by an interactive host after a discovery-time vision probe
    /// (config > probe > false). `Some(true)` lifts the image-attach preflight for
    /// a provider that did not declare vision in config but was probed as
    /// vision-capable; it never *removes* capability the declaration already
    /// advertises (official API or a config-declared local server). `None` (the
    /// default for every non-interactive host) keeps the declaration as the sole
    /// source, so behaviour is unchanged unless a host opts in.
    image_support_override: Option<bool>,
    /// Whether a turn is currently running. A switch is refused while this is set,
    /// so the provider/model are only ever re-pointed at a turn boundary.
    turn_in_flight: bool,
    /// Tool calls executed in the current turn. Reset at each turn start; folded
    /// into the per-turn handoff at the turn's exit.
    turn_tool_calls: usize,
    /// Workspace files written/edited/deleted in the current turn (best-effort,
    /// by file-write tool path argument). Reset at each turn start.
    turn_files_changed: Vec<String>,
    /// Whether the current turn persisted learning to memory. Reset at each turn
    /// start; the run-turn path only reads memory, so it stays `false` here.
    turn_memory_written: bool,
    /// The memories injected into the current turn's context. Reset at each turn
    /// start, set from the context contribution, and delivered to the context
    /// hooks once at the turn's exit (`stop`) for best-effort usage tracking —
    /// post-turn, never on the retrieval read path.
    turn_memories_used: Vec<localpilot_store::MemoryUsed>,
    /// The handoff summarizing the most recently finished turn, built at the
    /// single exit (`stop`). Read by a non-interactive caller for a terminal
    /// state even when the turn timed out.
    last_handoff: Option<TurnHandoff>,
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
            crate::system_prompt::agent_system_prompt(&tools, config.tool_marker_enabled),
        ));
        messages.extend(seed);

        let rule_engine = RuleEngine::with_baseline(&config.rules);

        let mut runtime = Self {
            provider,
            tools,
            engine: PermissionEngineHandle::new(engine),
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
            error_breaker: RepeatedErrorBreaker::default(),
            summarizer: None,
            rule_engine,
            named_targets: Vec::new(),
            broker: None,
            background: Arc::new(localpilot_tools::BackgroundProcesses::new()),
            registry: None,
            image_support_override: None,
            turn_in_flight: false,
            turn_tool_calls: 0,
            turn_files_changed: Vec::new(),
            turn_memory_written: false,
            turn_memories_used: Vec::new(),
            last_handoff: None,
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

    /// Evaluate the `check_before_launch` discipline rule for a tool call. When
    /// the prompt named a local serveable target that has **not** been probed this
    /// session and this call launches a local server or scaffolds a competing
    /// entry file, the rule's verdict (`Warn`/`Block`) is returned; otherwise
    /// `None`. Evidence-grounded — the probe state is read from the session ledger,
    /// never the model's claim — and tighten-only: it can refuse or warn, never
    /// grant. Returns `None` (no objection) when no target was named.
    fn check_before_launch_verdict(
        &self,
        name: &str,
        input: &serde_json::Value,
    ) -> Option<RuleVerdict> {
        if self.named_targets.is_empty() {
            return None;
        }
        let launch_or_scaffold_attempt = match name {
            "run_shell" => launch_targets::shell_command_line(input)
                .is_some_and(|command| launch_targets::is_launch_command(&command)),
            "write_file" | "apply_patch" => launch_targets::is_scaffold_write(name, input),
            _ => false,
        };
        if !launch_or_scaffold_attempt {
            return None;
        }
        let events = self.store.read_events(self.session_id).ok()?;
        let ledger = crate::evidence::EvidenceLedger::project(&events);
        let ctx = RuleContext {
            named_local_target_unprobed: launch_targets::any_target_unprobed(
                &self.named_targets,
                &ledger,
            ),
            launch_or_scaffold_attempt: true,
            ..RuleContext::default()
        };
        let trigger = if name == "run_shell" {
            Trigger::PreShell
        } else {
            Trigger::PreTool
        };
        self.rule_engine
            .evaluate(trigger, &ctx)
            .into_iter()
            .find_map(|(rule, verdict)| (rule == "check_before_launch").then_some(verdict))
    }

    /// Failure-driven re-resolution (ADR-0031): when the broker is on and `name`
    /// is not advertised (unknown to the registry, out of the working set, or
    /// retired), reveal the closest available tool and return the model-visible
    /// resolution to surface *in place of* dispatch — the attempted call never
    /// runs and the model retries. `None` when no broker is installed or the tool
    /// is already advertised (dispatch normally).
    fn broker_reresolution(&self, name: &str) -> Option<localpilot_tools::Resolution> {
        let broker = self.broker.as_ref()?;
        if broker.is_advertised(name) {
            return None;
        }
        Some(broker.reresolve(name))
    }

    /// Loose NL marker trigger (ADR-0031), gated on `tool_marker_enabled` and a
    /// live broker: parse assistant `text` for `NEED: <capability>` markers and
    /// reveal the closest tool for each, returning the revealed names. A no-op
    /// (empty) when disabled, so the marker costs nothing unless opted in.
    fn reveal_for_markers(
        &mut self,
        text: &str,
        events: &broadcast::Sender<RuntimeEvent>,
    ) -> Vec<String> {
        if !self.config.tool_marker_enabled {
            return Vec::new();
        }
        // Resolve every marker first (this borrows the broker), then record the
        // events (which needs `&mut self`), so the two borrows do not overlap.
        let resolutions: Vec<localpilot_tools::Resolution> = match self.broker.as_ref() {
            Some(broker) => parse_need_markers(text)
                .iter()
                .map(|need| broker.reresolve(need))
                .collect(),
            None => return Vec::new(),
        };
        let mut revealed = Vec::new();
        for resolution in resolutions {
            self.record_resolution(&resolution, "marker");
            if let Some(name) = resolution.revealed.clone() {
                let _ = events.send(RuntimeEvent::Warning(format!(
                    "revealed `{name}` for stated need: {}",
                    resolution.need
                )));
                revealed.push(name);
            }
        }
        revealed
    }

    /// Record a broker resolution to the durable session event log (redacted on
    /// append, ADR-0011; local and disposable, ADR-0012), gated on broker learning
    /// being enabled. With learning off, no telemetry is written.
    fn record_resolution(&mut self, resolution: &localpilot_tools::Resolution, trigger: &str) {
        if self
            .broker
            .as_ref()
            .is_some_and(localpilot_tools::Broker::learning_enabled)
        {
            self.record_event(SessionEventKind::ToolResolution {
                need: resolution.need.clone(),
                chosen: resolution.revealed.clone(),
                score: resolution.score,
                trigger: trigger.to_string(),
            });
        }
    }

    /// Verify an executed call against its contract and return the verdict label
    /// to record. Deterministic-first; a model critic is a future drop-in.
    fn verify_call(&self, name: &str, input: &serde_json::Value, result: &ToolResult) -> String {
        let verdict = match self.tools.get(name) {
            Some(tool) => {
                let contract = tool.contract();
                let observation = Observation::from_tool_result(result);
                DeterministicVerifier.verify(&VerificationInput {
                    contract: &contract,
                    input,
                    observation: &observation,
                    workspace: &self.workspace,
                })
            }
            None => Verdict::Unverified,
        };
        match verdict {
            Verdict::Verified => "verified",
            Verdict::Unverified => "unverified",
            Verdict::Failed => "failed",
        }
        .to_string()
    }

    /// Record whether a tool call's arguments validate against the tool's JSON
    /// schema, as redacted baseline telemetry, before the call is dispatched.
    /// Emits one [`SessionEventKind::ToolInputValid`] or
    /// [`SessionEventKind::ToolInputInvalid`] per call, carrying identifiers, the
    /// malformed class, the offending field path(s), and the JSON type seen there
    /// — never a raw argument value. A no-op for an unknown tool (no schema to
    /// check). Pure measurement: dispatch behaviour is unchanged either way.
    fn record_tool_input_validity(&mut self, name: &str, input: &serde_json::Value) {
        let Some(schema) = self.tools.get(name).map(localpilot_tools::Tool::schema) else {
            return;
        };
        let issues = localpilot_tools::tool_input_issues(&schema, input);
        let provider = self.active_provider_id().to_string();
        let model = self.config.model.clone();
        let kind = match issues.first() {
            Some(first) => SessionEventKind::ToolInputInvalid {
                tool: name.to_string(),
                provider,
                model,
                class: first.class.label().to_string(),
                issue_paths: issues.iter().map(|issue| issue.path.clone()).collect(),
                before_type: first.actual.clone(),
            },
            None => SessionEventKind::ToolInputValid {
                tool: name.to_string(),
                provider,
                model,
            },
        };
        self.record_event(kind);
    }

    /// Validate — and, when `[tools] repair` is enabled, repair — a tool call's
    /// arguments before dispatch. `None` for an unknown tool (the unknown-tool path
    /// answers it). The result drives the dispatch choice: a repaired input is
    /// dispatched in place of the original (with a model-visible note), a
    /// shape-invalid input yields a schema-aware readable error (when readable
    /// errors are on), and a valid input dispatches byte-unchanged. The readable
    /// message is value-free and redacted by the repair stage, so it cannot echo a
    /// secret.
    fn tool_input_decision(
        &self,
        name: &str,
        input: &serde_json::Value,
    ) -> Option<localpilot_tools::ToolInputValidationResult> {
        let tool = self.tools.get(name)?;
        let schema = tool.schema();
        let contract = tool.contract();
        let request = localpilot_tools::RepairRequest {
            tool: name,
            schema: &schema,
            side_effect: contract.side_effect,
            reversibility: contract.reversibility,
            is_mcp: self.tools.is_mcp(name),
            attempt_repair: self.config.repair_mode.is_enabled(),
            examples: contract.examples,
        };
        Some(localpilot_tools::evaluate_tool_input(&request, input))
    }

    /// Apply the no-unsupported-claim gate to a final reply, returning the
    /// (possibly rewritten) text. A no-op unless `enforce_claim_gate` is set.
    fn gate_final_reply(&self, text: String) -> String {
        if !self.config.enforce_claim_gate {
            return text;
        }
        let Ok(events) = self.store.read_events(self.session_id) else {
            return text;
        };
        let ledger = crate::evidence::EvidenceLedger::project(&events);
        crate::claim::review_final_reply(&text, &ledger).unwrap_or(text)
    }

    /// Record that this session is closing.
    pub fn close(&mut self) {
        self.background.kill_all();
        self.persist_graduation();
        self.record_event(SessionEventKind::SessionClosed);
    }

    /// Start a fresh session: a new id, a clean conversation (the setup
    /// system prompt is kept), and a new durable event chain.
    pub fn start_new_session(&mut self) {
        // A fresh session must not inherit the previous one's running servers.
        self.background.kill_all();
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
            processes: Some(self.background.as_ref()),
        };
        let engine = self.engine.snapshot();
        let result = self
            .tools
            .dispatch(&call, &ctx, &engine, self.approver.as_ref())
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

    /// Replace the active permission profile. Interactive hosts use this when
    /// a slash command changes profile between turns; the swap lands in the
    /// same shared handle [`Self::permission_engine_handle`] exposes, so both
    /// paths stay consistent.
    pub fn set_permission_profile(&mut self, profile: Profile, allowlist: Vec<String>) {
        self.engine.set(PermissionEngine::new(profile, allowlist));
    }

    /// The shared, swappable permission-engine handle. An interactive host
    /// clones it before starting a turn so a profile slash command can apply
    /// *while the model is generating* — the runtime snapshots the engine per
    /// tool call, so the swap takes effect from the next call without needing
    /// the mutable runtime borrow the in-flight turn holds.
    #[must_use]
    pub fn permission_engine_handle(&self) -> PermissionEngineHandle {
        self.engine.clone()
    }

    /// Install the pull-discovery broker (ADR-0031). When set, the per-turn tool
    /// specs are narrowed to the broker's working set (the advertise lever) and
    /// the failure-driven / marker triggers feed it. The host builds the broker,
    /// registers its `tool_search`/`tool_load` tools in the registry, and seeds its
    /// catalog before handing it here.
    ///
    /// When the broker learns, the graduated tools from prior sessions are seeded
    /// from the local, disposable store so a common need is advertised from turn
    /// one (ADR-0012). Best-effort: a missing or unreadable record is ignored.
    pub fn set_broker(&mut self, broker: Option<Broker>) {
        if let Some(broker) = &broker {
            if broker.learning_enabled() {
                if let Ok(Some(json)) = self.store.get_tool_output(GRADUATION_KEY) {
                    if let Ok(names) = serde_json::from_str::<Vec<String>>(&json) {
                        broker.seed_graduated(&names);
                    }
                }
            }
        }
        self.broker = broker;
    }

    /// Persist the broker's graduated tools to the local, disposable store so the
    /// next session advertises them from turn one. Best-effort and gated on broker
    /// learning; a write failure is logged, never fatal.
    fn persist_graduation(&self) {
        if let Some(broker) = &self.broker {
            if broker.learning_enabled() {
                if let Ok(json) = serde_json::to_string(&broker.graduated_names()) {
                    if let Err(err) = self.store.put_tool_output(GRADUATION_KEY, &json) {
                        tracing::warn!(error = %err, "failed to persist tool graduation");
                    }
                }
            }
        }
    }

    /// Set the reasoning effort for subsequent turns — switchable from the
    /// REPL, and overridable per harness step (high for planning, low for
    /// mechanical edits).
    pub fn set_reasoning_effort(&mut self, effort: Option<localpilot_llm::ReasoningEffort>) {
        self.config.reasoning_effort = effort;
    }

    /// Enable (or disable) the verify-before-done gate at runtime, optionally
    /// overriding the verification command. Used by `eval --verify` so a
    /// benchmark arm can turn the gate on without a config file; an explicit
    /// `command` (when `Some`) overrides any stack detection. Leaves the command
    /// untouched when `None`, so a config-set command survives a flag that only
    /// flips the gate on.
    pub fn set_verify_before_done(&mut self, enabled: bool, command: Option<String>) {
        self.config.verify_before_done = enabled;
        if command.is_some() {
            self.config.verify_command = command;
        }
    }

    /// The currently requested reasoning effort.
    #[must_use]
    pub fn reasoning_effort(&self) -> Option<localpilot_llm::ReasoningEffort> {
        self.config.reasoning_effort
    }

    /// Attach the already-built provider registry so the active provider/model can
    /// be re-pointed mid-session. The host builds every configured provider once
    /// (`ProviderRegistry::from_config`) and hands the runtime a shared handle;
    /// the switch then selects an already-built provider rather than rebuilding or
    /// re-authenticating one. Without a registry attached, a switch is refused.
    pub fn set_registry(&mut self, registry: Arc<ProviderRegistry>) {
        self.registry = Some(registry);
    }

    /// The id of the active provider, read from its own declaration.
    #[must_use]
    pub fn active_provider_id(&self) -> &str {
        &self.provider.declaration().id
    }

    /// Whether the active provider accepts image input, so the UI can offer (or
    /// refuse) pasting an image. True when the provider *declares* image input
    /// (the official API, or a config-declared local server) **or** an interactive
    /// host recorded a positive probe override. The override only ever adds
    /// capability, so it cannot turn off images for a provider that already
    /// declares them.
    #[must_use]
    pub fn active_accepts_images(&self) -> bool {
        self.provider
            .declaration()
            .supported_input_blocks
            .contains(&InputBlockKind::Image)
            || self.image_support_override == Some(true)
    }

    /// Record the probe-resolved image-input capability for the active provider
    /// (config > probe > false), set by an interactive host after a discovery-time
    /// vision probe. Only `Some(true)` changes behaviour (it lifts the image-attach
    /// preflight for an undeclared-but-probed-vision provider); `Some(false)`/`None`
    /// leaves the provider's own declaration as the sole gate.
    pub fn set_image_support_override(&mut self, resolved: Option<bool>) {
        self.image_support_override = resolved;
    }

    /// The active model.
    #[must_use]
    pub fn active_model(&self) -> &str {
        &self.config.model
    }

    /// Re-point the session at the configured provider `id`, selecting the
    /// already-built provider from the attached registry. The transcript
    /// (`Vec<Message>`) is provider-neutral and is left untouched, so the
    /// conversation continues against the new provider on the next turn. The model
    /// follows the provider: the new provider's configured default model is used,
    /// or — when it has none — the current model name is kept and a warning is
    /// returned. Refused while a turn is in flight, or when `id` is not configured.
    ///
    /// # Errors
    /// [`SwitchError::TurnInFlight`] mid-turn; [`SwitchError::UnknownProvider`]
    /// when no registry is attached or `id` is not configured.
    pub fn set_active_provider(&mut self, id: &str) -> Result<SwitchOutcome, SwitchError> {
        if self.turn_in_flight {
            return Err(SwitchError::TurnInFlight);
        }
        let registry = self
            .registry
            .as_ref()
            .ok_or_else(|| SwitchError::UnknownProvider(id.to_string()))?;
        let provider = registry
            .get(id)
            .ok_or_else(|| SwitchError::UnknownProvider(id.to_string()))?
            .clone();
        let (model, warning) = match registry.default_model(id) {
            Some(model) => (model.to_string(), None),
            None => (
                self.config.model.clone(),
                Some(format!(
                    "provider '{id}' has no configured default model; keeping '{}'",
                    self.config.model
                )),
            ),
        };
        self.provider = provider;
        self.config.model = model.clone();
        Ok(SwitchOutcome {
            provider_id: id.to_string(),
            model,
            warning,
        })
    }

    /// Set the active model for subsequent turns on the current provider. Used for
    /// `/model <provider> <model>` (after [`set_active_provider`]) and for a
    /// model-only change. The provider and transcript are untouched. Model-id
    /// validity against the provider's catalog is the caller's concern (the
    /// `/model` UX validates against discovery); this only re-points the name.
    ///
    /// # Errors
    /// [`SwitchError::TurnInFlight`] when a turn is in flight.
    pub fn set_active_model(&mut self, model: impl Into<String>) -> Result<(), SwitchError> {
        if self.turn_in_flight {
            return Err(SwitchError::TurnInFlight);
        }
        self.config.model = model.into();
        Ok(())
    }

    /// A clonable handle for queueing steering input into a running turn.
    /// Queued text is admitted at the next safe provider-turn boundary.
    #[must_use]
    pub fn steer_queue(&self) -> SteerQueue {
        self.steer.clone()
    }

    /// A clonable handle to the background-process registry, so the UI can list
    /// and stop processes while a turn is in flight (the registry is
    /// interior-mutable behind a single lock).
    #[must_use]
    pub fn background_handle(&self) -> Arc<localpilot_tools::BackgroundProcesses> {
        Arc::clone(&self.background)
    }

    /// A borrowed view of the background-process registry, for the idle-path UI
    /// commands that already hold the runtime.
    #[must_use]
    pub fn background_registry(&self) -> &localpilot_tools::BackgroundProcesses {
        self.background.as_ref()
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
        let mut outcomes = Vec::new();
        for check in checks {
            if trigger_for_cadence(check.cadence) == trigger {
                let outcome = self.run_check(check, root).await;
                self.hooks.notify(&HookEvent::GateCheck {
                    name: outcome.name.clone(),
                    passed: outcome.passed(),
                });
                outcomes.push(outcome);
            }
        }
        outcomes
    }

    /// Run one check command through the permission-gated [`CheckRunner`] and
    /// return its outcome — the single seam shared by the step-cadence quality
    /// gate (`run_gate_checks`) and the verify-before-done gate, so there is no
    /// second command-running path.
    async fn run_check(&self, check: &CheckConfig, root: &Path) -> CheckOutcome {
        let engine = self.engine.snapshot();
        let runner = CheckRunner::new(
            &engine,
            self.approver.as_ref(),
            self.config.interactivity,
            self.config.trusted,
            root,
        );
        runner.run(check).await
    }

    /// The verify-before-done gate, consulted when a turn would finalize with no
    /// tool call. Reuses [`Self::run_check`] — the same runner the quality gate
    /// uses — so it never runs a second compile engine. The outcome tells the
    /// caller how to end (or continue) the turn:
    /// - [`VerifyGate::Finalize`] — gate off, no target, verification passed, or
    ///   the command could not run: finalize the turn as `Done`.
    /// - [`VerifyGate::Retry`] — verification failed and a re-entry remains: feed
    ///   the diagnostics back and keep going.
    /// - [`VerifyGate::GiveUp`] — the re-entry cap was reached with the build
    ///   still failing: stop the turn with `NoProgress` rather than accept a
    ///   never-green "done". This ties the no-progress stop to the verify signal.
    async fn verify_before_done(
        &self,
        attempts: &mut usize,
        events: &broadcast::Sender<RuntimeEvent>,
    ) -> VerifyGate {
        if !self.config.verify_before_done {
            return VerifyGate::Finalize;
        }
        // The gate runs its build/test command as a child process, so it needs the
        // de-verbatim spawn cwd (see `Workspace::process_dir`): handed a verbatim
        // `\\?\` working directory the gate would compile in `C:\Windows`, not the
        // workspace. Detection reads marker files, which resolve the same on either
        // spelling.
        let root = self.workspace.process_dir();
        let Some(check) = crate::resolve_verify_check(&root, self.config.verify_command.as_deref())
        else {
            // The gate is on but no build/test target was detected (and no
            // override): finalize unchanged, but surface it so a silently
            // un-verified solve is visible rather than mistaken for a pass.
            let _ = events.send(RuntimeEvent::Warning(
                "verify-before-done: no build/test target detected for this workspace; \
                 finalizing without a verify signal (set `verify_command` to force one)"
                    .to_string(),
            ));
            return VerifyGate::Finalize;
        };
        if *attempts >= VERIFY_GATE_MAX_ATTEMPTS {
            let _ = events.send(RuntimeEvent::Warning(format!(
                "verify-before-done: still failing after {VERIFY_GATE_MAX_ATTEMPTS} attempts; \
                 stopping (no forward progress toward a passing build)"
            )));
            return VerifyGate::GiveUp;
        }
        let outcome = self.run_check(&check, &root).await;
        match outcome.status {
            CheckStatus::Passed => VerifyGate::Finalize,
            // An environment problem (denied or unstartable command) must not
            // wedge a finished turn: record it and finalize without a signal.
            CheckStatus::Denied | CheckStatus::Errored => {
                let _ = events.send(RuntimeEvent::Warning(format!(
                    "verify-before-done: could not run `{}` ({:?}); finalizing without a verify signal",
                    check.program, outcome.status
                )));
                VerifyGate::Finalize
            }
            CheckStatus::Failed => {
                *attempts += 1;
                let _ = events.send(RuntimeEvent::Warning(format!(
                    "verify-before-done: `{}` failed (attempt {}/{VERIFY_GATE_MAX_ATTEMPTS}); feeding diagnostics back",
                    check.program, *attempts
                )));
                VerifyGate::Retry(format!(
                    "The build/test verification did not pass, so the task is not yet complete. \
                     Fix the problem and continue.\n\n{}",
                    outcome.detail
                ))
            }
        }
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
                        if err.is_context_length_error() {
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
            // The advertise lever: with the broker on, only the working set's
            // schemas reach the provider (core ∪ broker tools ∪ revealed); with it
            // off, every registered tool is advertised (today's behaviour).
            .filter(|(name, _, _)| match &self.broker {
                Some(broker) => broker.is_advertised(name),
                None => true,
            })
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
        self.run_turn_with_attachments(user_input, &[], events, cancel)
            .await
    }

    /// Run a turn whose opening user message carries `attachments` (e.g. pasted
    /// image blocks) alongside the typed `user_input` text. The text still drives
    /// hooks, target extraction, and token estimation; the attachments ride only
    /// in the user message sent to the provider.
    pub async fn run_turn_with_attachments(
        &mut self,
        user_input: &str,
        attachments: &[ContentBlock],
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
        // One retrieval yields both the injected text and the exact memories it
        // represents, so the "memories used" record cannot diverge from what was
        // injected. Recording is pure observation — it never changes the context,
        // and an empty set records nothing.
        // A turn is now in flight: a mid-session provider/model switch is refused
        // until it ends (cleared in `stop`, the single exit), so the transcript is
        // only ever re-pointed at a turn boundary.
        self.turn_in_flight = true;
        self.turn_tool_calls = 0;
        self.turn_files_changed.clear();
        self.turn_memory_written = false;
        self.turn_memories_used.clear();
        // A bounded per-turn deadline, when configured: the turn stops cleanly with
        // a handoff at this instant rather than hanging. `None` leaves it unbounded.
        let deadline = self
            .config
            .turn_timeout
            .map(|timeout| tokio::time::Instant::now() + timeout);
        let contribution = self.hooks.contribute(user_input);
        let retrieval_text = contribution.text.unwrap_or_default();
        if !contribution.memories.is_empty() {
            // Stash the injected set for a single best-effort usage bump at the
            // turn's exit (`stop`) — post-turn, off the retrieval read path.
            self.turn_memories_used = contribution.memories.clone();
            self.record_event(SessionEventKind::MemoriesUsed {
                memories: contribution.memories,
            });
        }
        let turn_context = (!retrieval_text.is_empty())
            .then(|| Message::new(Role::System, vec![ContentBlock::text(retrieval_text)]));
        let context_reserve = turn_context
            .as_ref()
            .map_or(0, |message| estimate_tokens(std::slice::from_ref(message)));
        // Record any local serveable target this turn's prompt named, so a later
        // launch/scaffold can be checked against a prior probe of it.
        for target in launch_targets::extract_targets(user_input) {
            if !self.named_targets.contains(&target) {
                self.named_targets.push(target);
            }
        }
        if attachments.is_empty() {
            self.append(Message::text(Role::User, user_input));
        } else {
            let mut blocks = Vec::with_capacity(attachments.len() + 1);
            blocks.push(ContentBlock::text(user_input));
            blocks.extend(attachments.iter().cloned());
            self.append(Message::new(Role::User, blocks));
        }
        self.last_quota = None;
        self.tool_failure_guard.reset();
        self.error_breaker.reset();
        let mut tools_enabled = true;
        // A provider may reject a request as too large even when the local
        // estimate believed it fit. The first overflow forces a tighter
        // compaction and one retry; a second overflow this turn is terminal.
        let mut overflow_retried = false;
        // A mid-stream truncation (the server dropped the response before it
        // completed) is a transient infrastructure fault, retried up to
        // `max_stream_retries` across turn iterations before giving up honestly.
        let mut stream_truncated_retries: u32 = 0;
        // Total tool calls executed this turn, bounded by the budget controller.
        let mut tool_calls_used = 0usize;
        // Progress-aware ceiling: a productive turn extends to the hard max; a
        // turn detected as spinning stops at the soft start; the hard max always
        // stops the loop. Both reset per turn (these are fresh per `run_turn`).
        let budget = BudgetController::new(
            self.config.tool_call_budget,
            self.config.tool_call_budget_max,
        );
        let mut no_progress = NoProgressDetector::default();
        // Always-on degenerate-loop guard: consecutive failing calls with no
        // successful call between them. Resets on any successful call. Unlike the
        // opt-in budget, this bounds a spin even when the budget is off.
        let mut unproductive_streak = 0usize;
        // Verify-before-done re-entries used this turn, capped so the gate can
        // never loop forever even with the rails off.
        let mut verify_attempts = 0usize;

        loop {
            if cancel.is_cancelled() {
                return self.stop(events, StopReason::Cancelled);
            }
            if deadline.is_some_and(|dl| tokio::time::Instant::now() >= dl) {
                return self.stop(events, StopReason::TimedOut);
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
            // For a constrained-decoding provider, derive a tool-call constraint
            // from the tools' schemas so arguments are schema-valid by
            // construction; `None` for every other provider (unchanged behaviour).
            let tool_constraint =
                localpilot_llm::constraint_for(self.provider.declaration(), &tools);
            let request = ModelRequest::new(self.config.model.clone(), request_messages)
                .with_tools(tools)
                .with_reasoning_effort(self.config.reasoning_effort)
                .with_tool_constraint(tool_constraint);

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
            let mut calls: Vec<(String, String, serde_json::Value, Option<serde_json::Value>)> =
                Vec::new();
            let mut stream_failed = false;
            // When a tool call's arguments fail to parse, the provider reports
            // which tool — kept so a malformed *write* steers recovery to a
            // chunked retry rather than a blind re-prompt.
            let mut failed_tool: Option<String> = None;
            let mut output_limited = false;
            let mut overflow = false;
            // The server dropped the stream before completing the response. Unlike
            // a malformed turn, this is retried transiently, not fed to the
            // bad-output recovery ladder.
            let mut truncated = false;
            // Live degenerate-output guard, fed incrementally so a runaway
            // stream is aborted early without rescanning the whole turn.
            let mut monitor = StreamMonitor::default();

            loop {
                tokio::select! {
                    () = cancel.cancelled() => {
                        return self.stop(events, StopReason::Cancelled);
                    }
                    // Bounded turn deadline (when configured). `sleep_until` targets
                    // an absolute instant, so re-arming it each iteration does not
                    // drift; with no deadline the branch parks forever and never fires.
                    () = async {
                        match deadline {
                            Some(dl) => tokio::time::sleep_until(dl).await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        return self.stop(events, StopReason::TimedOut);
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
                        Some(Ok(ModelEvent::ToolCall {
                            id,
                            name,
                            input_json,
                            provider_metadata,
                        })) => {
                            calls.push((id, name, input_json, provider_metadata));
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
                            // A mid-stream truncation (the server dropped the
                            // response before completing it — a local server that
                            // hung, crashed, or ran out of VRAM) is an
                            // infrastructure fault, not a bad model turn: it must
                            // not spend recovery health toward Degraded or trigger
                            // a "malformed output" repair aimed at a server that
                            // already dropped. Retry the whole request instead.
                            if matches!(err, ProviderError::StreamTruncated { .. }) {
                                truncated = true;
                                break;
                            }
                            let _ = events
                                .send(RuntimeEvent::Warning(format!("stream error: {err}")));
                            // A context-length rejection mid-stream is an
                            // overflow, not a fatal turn error: shrink and retry
                            // once before giving up.
                            if err.is_context_length_error() {
                                overflow = true;
                                break;
                            }
                            if stream_error_stops_turn(&err) {
                                return self.stop(events, StopReason::ProviderError);
                            }
                            if let ProviderError::MalformedToolArguments { tool, .. } = &err {
                                failed_tool = Some(tool.clone());
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

            if truncated {
                // The server cut the stream mid-response. The partial text/calls
                // this turn are discarded (never persisted), so re-issuing the
                // same request is safe. Retry with backoff up to the transient
                // ceiling, then stop with an honest reason — not a Degraded/
                // malformed-output verdict the server, not the model, earned.
                if stream_truncated_retries < self.config.max_stream_retries {
                    stream_truncated_retries += 1;
                    let max = self.config.max_stream_retries;
                    let secs = 1u64 << (stream_truncated_retries - 1).min(5);
                    let _ = events.send(RuntimeEvent::Warning(format!(
                        "the model server ended the response early (it may have hung, crashed, \
                         or run out of VRAM); retrying {stream_truncated_retries}/{max} in {secs}s"
                    )));
                    tokio::select! {
                        () = cancel.cancelled() => return self.stop(events, StopReason::Cancelled),
                        () = tokio::time::sleep(Duration::from_secs(secs)) => {}
                    }
                    continue;
                }
                let _ = events.send(RuntimeEvent::Warning(
                    "the model server kept ending responses early; stopping this turn — check the \
                     local server (it may have crashed or run out of VRAM)"
                        .to_string(),
                ));
                return self.stop(events, StopReason::ProviderError);
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
                // Act on the input-shrink actions the ladder emits on a repeated
                // bad turn: compact active history (which also truncates oversized
                // tool results) so the retry sees a smaller context.
                if diagnostic.actions.iter().any(|action| {
                    matches!(
                        action,
                        RecoveryAction::ReduceContext
                            | RecoveryAction::SummarizeOversizedToolResults
                    )
                }) {
                    self.shrink_for_overflow(cancel).await;
                }
                // Choose the repair prompt. When the ladder asks for a chunked
                // write and the failed call was a file-write tool, steer the
                // model to write in pieces; otherwise use the generic prompt.
                let chunk_write = diagnostic
                    .actions
                    .contains(&RecoveryAction::RequestChunkedWrite)
                    && failed_tool.as_deref().is_some_and(is_file_write_tool);
                let prompt = if chunk_write {
                    CHUNKED_WRITE_REPAIR_PROMPT
                } else {
                    REPAIR_PROMPT
                };
                // Persisted and marked synthetic: the repair prompt shapes the
                // conversation the model sees, so a resumed session must
                // reconstruct it.
                self.append(Message::text(Role::User, prompt).into_synthetic("repair prompt"));
                continue;
            }
            self.recovery.record_clean_turn();

            // Validate the batch before persisting: a `tool_use` block with a
            // blank id can never be answered by a `tool_result`, so it must
            // not enter history at all. Every persisted `tool_use` is
            // guaranteed an answer on every exit path below.
            let rejection = invalid_tool_calls(&calls);
            let calls: Vec<(String, String, serde_json::Value, Option<serde_json::Value>)> =
                if rejection.is_some() {
                    calls
                        .into_iter()
                        .filter(|(id, _, _, _)| !id.trim().is_empty())
                        .collect()
                } else {
                    calls
                };

            // Assemble and persist the assistant message.
            let mut content = Vec::new();
            let reasoning = trim_blank_boundary_lines(reasoning);
            let text = trim_blank_boundary_lines(text);
            // When this turn ends without more tool calls, the text is the final
            // reply: the no-unsupported-claim gate reviews it (no-op when off).
            let text = if calls.is_empty() {
                self.gate_final_reply(text)
            } else {
                text
            };

            // Loose NL marker trigger (ADR-0031, gated): act on any `NEED:` marker
            // in the assistant text now, while it is still owned, revealing the
            // closest tool so the next turn advertises it. Empty unless opted in.
            let marker_revealed = self.reveal_for_markers(&text, events);

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
            for (id, name, input, provider_metadata) in &calls {
                let mut call =
                    ToolCall::new(ToolUseId::from(id.as_str()), name.clone(), input.clone());
                if let Some(metadata) = provider_metadata.clone() {
                    call = call.with_provider_metadata(metadata);
                }
                content.push(ContentBlock::ToolUse(call));
            }
            if !content.is_empty() {
                self.append(Message::new(Role::Assistant, content));
            }

            if let Some(reason) = rejection {
                let _ = events.send(RuntimeEvent::Warning(reason.clone()));
                // Answer every persisted tool_use so the wire contract holds,
                // carrying the rejection reason back to the model.
                for (id, _, _, _) in &calls {
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
                // The marker trigger may have revealed tools for a stated need
                // even though the model made no call this turn: keep the turn going
                // so it can use them, instead of ending. Without a reveal, a
                // call-free turn is the final answer.
                if !marker_revealed.is_empty() {
                    self.append(
                        Message::text(
                            Role::User,
                            format!(
                                "Revealed for your stated need: {}. They are now advertised — \
                                 call one to continue, or say you are done.",
                                marker_revealed.join(", ")
                            ),
                        )
                        .into_synthetic("tool marker reveal"),
                    );
                    continue;
                }
                // Verify-before-done gate (opt-in): before accepting a call-free
                // turn as the final answer, confirm the workspace still
                // builds/tests. On a failure within the re-entry cap, feed the
                // diagnostics back and keep going instead of "finishing" code
                // that never compiled. When the cap is reached with the build
                // still red, stop with `NoProgress` (the verify signal driving the
                // no-progress guard) rather than accept a never-green "done".
                // Bounded by the budget/timeout rails and `VERIFY_GATE_MAX_ATTEMPTS`.
                match self.verify_before_done(&mut verify_attempts, events).await {
                    VerifyGate::Finalize => return self.stop(events, StopReason::Done),
                    VerifyGate::Retry(feedback) => {
                        self.append(
                            Message::text(Role::User, feedback).into_synthetic("verify gate"),
                        );
                        continue;
                    }
                    VerifyGate::GiveUp => return self.stop(events, StopReason::NoProgress),
                }
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
            for (id, name, input, _) in &calls {
                // Progress-aware ceiling: a runaway or spinning tool loop stops
                // cleanly with a model-visible, recorded reason before the next
                // call runs. The hard cost ceiling always wins; a no-progress
                // stop is distinct so it is diagnosable.
                // Always-on degenerate-loop guard. A turn that is provably not
                // progressing must still stop instead of spinning to the ceiling:
                // a tripped no-progress detector (a repeated or cyclic call set) or
                // a long run of consecutive failing calls ends the turn. This stays
                // active for both no budget and the *built-in* default rail
                // (ADR-0055) — the built-in `..._max` with no soft start collapses
                // to `soft == hard`, which kills the controller's no-progress branch
                // below, so without this guard a stuck turn would burn the whole
                // ceiling. Only an *explicit* operator budget hands the no-progress
                // stop to the controller. See ADR-0052.
                if !self.config.tool_budget_explicit
                    && (no_progress.is_tripped() || unproductive_streak >= UNPRODUCTIVE_CALL_LIMIT)
                {
                    let notice = "no forward progress this turn (repeated or failing \
                                  calls); stopping instead of spinning"
                        .to_string();
                    let _ = events.send(RuntimeEvent::Warning(notice.clone()));
                    self.append(
                        Message::text(Role::User, notice).into_synthetic("no tool-call progress"),
                    );
                    return self.stop(events, StopReason::NoProgress);
                }
                match budget.decide(tool_calls_used, no_progress.is_tripped()) {
                    BudgetDecision::Continue => {}
                    BudgetDecision::StopCostMax => {
                        // Only `Bounded` returns `StopCostMax`, so the ceiling is
                        // always present here; fall back to the count defensively.
                        let ceiling = budget.hard_max().unwrap_or(tool_calls_used);
                        let notice = format!(
                            "tool-call budget of {ceiling} reached this turn; \
                             stopping to bound cost"
                        );
                        let _ = events.send(RuntimeEvent::Warning(notice.clone()));
                        self.append(
                            Message::text(Role::User, notice)
                                .into_synthetic("tool-call budget exceeded"),
                        );
                        return self.stop(events, StopReason::BudgetExceeded);
                    }
                    BudgetDecision::StopNoProgress => {
                        let notice = format!(
                            "no forward progress across {tool_calls_used} tool calls this turn; \
                             stopping instead of spinning"
                        );
                        let _ = events.send(RuntimeEvent::Warning(notice.clone()));
                        self.append(
                            Message::text(Role::User, notice)
                                .into_synthetic("no tool-call progress"),
                        );
                        return self.stop(events, StopReason::NoProgress);
                    }
                }
                tool_calls_used += 1;
                self.turn_tool_calls = tool_calls_used;

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
                // Light up the dormant validity metric: record, as redacted
                // baseline telemetry, whether this call's arguments validate
                // against the tool schema. Measurement only — dispatch is
                // unchanged whatever the verdict.
                self.record_tool_input_validity(name, input);
                // Cancellation races the executing tool: an abort synthesizes
                // an error result (the pairing contract holds), and dropping
                // the dispatch future drops spawned children, which are
                // configured to die with it instead of waiting out their
                // timeout. The aborted execution stays in the event log.
                // The look-before-launch discipline: if the prompt named a local
                // target not yet probed and this call launches its own server or
                // scaffolds a competing page, the rule warns (model-visible, the
                // call still runs) or, when configured to block, refuses it.
                // Compose the pre-execution gate chain into one ordered decision
                // (precedence: broker redirect → precondition block → rule block →
                // proceed). A refusal short-circuits before any advisory `Warn`, so
                // the ordering is pinned and tested in `dispatch_gate`, not implicit
                // here. The gate inputs are read-only; the permission engine runs
                // only on `Proceed`.
                let decision = pre_dispatch_decision(
                    self.broker_reresolution(name),
                    self.precondition_block(name, input),
                    self.check_before_launch_verdict(name, input),
                );
                // Phase 1 + 2: validate the arguments; when `[tools] repair` is on,
                // repair a shape-invalid call to a valid shape and dispatch that;
                // otherwise (and on an unrepairable or refused call) hand the model
                // a schema-aware error when readable errors are on. A valid call
                // dispatches byte-unchanged, exactly as before. Repaired and invalid
                // are mutually exclusive outcomes of the one validate-or-repair pass.
                let tool_decision = self.tool_input_decision(name, input);
                let repaired_input = tool_decision.as_ref().and_then(|d| {
                    matches!(d.outcome, localpilot_tools::RepairOutcome::Repaired)
                        .then(|| d.repaired_input.clone())
                        .flatten()
                });
                let readable_error = tool_decision.as_ref().and_then(|d| {
                    (matches!(d.outcome, localpilot_tools::RepairOutcome::Invalid)
                        && self.config.enforce_readable_errors)
                        .then(|| d.readable_message.clone())
                        .flatten()
                });
                let mut launch_warn: Option<String> = None;
                let mut readable_error_sent = false;
                let mut repaired_dispatched = false;
                let result = match decision {
                    PreDispatch::Redirect(resolution) => {
                        // The broker narrowed the surface and this tool is not
                        // advertised (unknown, out-of-working-set, or retired):
                        // reveal the closest available tool and ask the model to
                        // retry. The attempted call does not run (reveal-never-grant;
                        // no resolve-and-run). Surfaced as a non-error redirect.
                        let _ = events.send(RuntimeEvent::Warning(format!(
                            "tool `{name}` is not advertised; resolving to the closest available tool"
                        )));
                        self.record_resolution(&resolution, "failure_driven");
                        Some(localpilot_core::ToolResult::success(
                            ToolUseId::from(id.as_str()),
                            resolution.message,
                        ))
                    }
                    PreDispatch::Block { reason, announce } => {
                        // A contract precondition (quiet) or a blocking
                        // `check_before_launch` severity (announced) refused the call
                        // before permission. Tighten-only: surface it as an error
                        // result so the model sees why and can recover.
                        if announce {
                            let _ = events.send(RuntimeEvent::Warning(reason.clone()));
                        }
                        Some(localpilot_core::ToolResult::error(
                            ToolUseId::from(id.as_str()),
                            reason,
                        ))
                    }
                    PreDispatch::Proceed { warn } => {
                        launch_warn = warn;
                        if let Some(message) = &readable_error {
                            // Deliver the schema-aware error as the tool result;
                            // the call never reaches dispatch (it could not run).
                            readable_error_sent = true;
                            Some(localpilot_core::ToolResult::error(
                                ToolUseId::from(id.as_str()),
                                message.clone(),
                            ))
                        } else {
                            // Dispatch the repaired input in place of the original
                            // when a repair fired; otherwise the model's own input.
                            // The permission engine + gates run on whichever is sent
                            // (reveal-never-grant: repair changes args, not authority).
                            let active_input = match &repaired_input {
                                Some(repaired) => {
                                    repaired_dispatched = true;
                                    repaired.clone()
                                }
                                None => input.clone(),
                            };
                            let active_call = ToolCall::new(
                                ToolUseId::from(id.as_str()),
                                name.clone(),
                                active_input,
                            );
                            let retention = StoreRetention(&self.store);
                            let ctx = ToolContext {
                                workspace: &self.workspace,
                                interactivity: self.config.interactivity,
                                trusted: self.config.trusted,
                                retention: Some(&retention),
                                processes: Some(self.background.as_ref()),
                            };
                            let gates = self.hooks.gates();
                            // Snapshot per call: a mid-turn profile swap through
                            // the shared handle applies from the next tool call.
                            let engine = self.engine.snapshot();
                            tokio::select! {
                                () = cancel.cancelled() => None,
                                result = self.tools.dispatch_gated(
                                    &active_call,
                                    &ctx,
                                    &engine,
                                    self.approver.as_ref(),
                                    &gates,
                                ) => Some(result),
                            }
                        }
                    }
                };
                // Safety-gate audit: a refusal to repair a destructive/external/
                // irreversible/MCP tool is recorded even when readable errors are
                // off, so the gate holding is always observable.
                if tool_decision.as_ref().is_some_and(|d| d.rejected_high_risk) {
                    let provider = self.active_provider_id().to_string();
                    let model = self.config.model.clone();
                    let risk = tool_decision
                        .as_ref()
                        .map(|d| format!("{:?}", d.risk).to_lowercase())
                        .unwrap_or_default();
                    self.record_event(SessionEventKind::ToolRepairRejectedHighRisk {
                        tool: name.clone(),
                        provider,
                        model,
                        risk,
                    });
                }
                // The readable-error recovery rung (sibling of the chunked-write
                // rung): record that a schema-aware correction was sent so it is
                // observable, and let the model retry on the next turn. It is
                // non-degrading — bounded by the per-turn tool-call budget, not
                // the bad-output degrade counter.
                if readable_error_sent {
                    debug_assert_eq!(
                        self.recovery.tool_input_repair_rung(),
                        localpilot_recovery::RecoveryAction::RepairToolArguments
                    );
                    let provider = self.active_provider_id().to_string();
                    let model = self.config.model.clone();
                    self.record_event(SessionEventKind::ToolInputRetryMessageSent {
                        tool: name.clone(),
                        provider,
                        model,
                    });
                    let _ = events.send(RuntimeEvent::Warning(format!(
                        "`{name}` arguments did not match its schema; sent a schema-aware \
                         correction for the model to retry"
                    )));
                }
                let Some(mut result) = result else {
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

                // A repaired call ran with rewritten arguments: attach the
                // model-visible note (so the model sees what changed and learns the
                // right shape), emit the redacted repair telemetry, and — in `warn`
                // mode — log the repair loudly so it can be vetted before `on`.
                if repaired_dispatched {
                    if let Some(decision) = &tool_decision {
                        if let Some(note) = &decision.model_note {
                            result
                                .output
                                .push_str(&format!("\n\n[arguments repaired] {note}"));
                        }
                        let provider = self.active_provider_id().to_string();
                        let model = self.config.model.clone();
                        let class = decision
                            .issues
                            .first()
                            .map(|issue| issue.class.label().to_string())
                            .unwrap_or_default();
                        let rules = decision
                            .repairs_applied
                            .iter()
                            .map(|rule| (*rule).to_string())
                            .collect();
                        self.record_event(SessionEventKind::ToolInputRepaired {
                            tool: name.clone(),
                            provider,
                            model,
                            class,
                            rules,
                        });
                        if self.config.repair_mode.is_loud() {
                            if let Some(note) = &decision.model_note {
                                let _ = events.send(RuntimeEvent::Warning(format!(
                                    "[repair] `{name}`: {note}"
                                )));
                            }
                        }
                    }
                }

                // Scrub raw control bytes from tool output before it reaches the
                // model context, the event stream, or the same-error breaker.
                // Binary-laden output (e.g. a parser error echoing raw `.glb`
                // bytes) otherwise carries NUL and other control characters that
                // can derail local models into a degenerate echo loop.
                if let Some(scrubbed) = scrub_control_chars(&result.output) {
                    result.output = scrubbed;
                }

                // Record a successful workspace mutation for the per-turn handoff,
                // so a timed-out or cut-off run still reports which files it touched.
                if !result.is_error && is_file_write_tool(name) {
                    if let Some(path) = file_write_path(input) {
                        if !self.turn_files_changed.contains(&path) {
                            self.turn_files_changed.push(path);
                        }
                    }
                }

                // A `Warn` from check-before-launch lets the call run but appends
                // the nudge to its result so the model reads it and can probe the
                // named target before launching again. (A `Block` already refused
                // above via the decision and never reaches here as a Warn.)
                if let Some(message) = &launch_warn {
                    let _ = events.send(RuntimeEvent::Warning(message.clone()));
                    result
                        .output
                        .push_str(&format!("\n\n[check-before-launch] {message}"));
                }

                // Track per-tool failure counts for the safeguard.
                if result.is_error {
                    unproductive_streak += 1;
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
                    // Same-error breaker: when a tool fails identically several
                    // times in a row, force a strategy change *before* the failure
                    // budget is spent by surfacing a hint in the model-visible
                    // result, rather than letting it re-send the same call.
                    if self.error_breaker.observe(name, &result.output) {
                        let hint = same_error_hint(name);
                        let _ = events.send(RuntimeEvent::Warning(format!(
                            "tool `{name}` keeps failing the same way; nudging a strategy change"
                        )));
                        result.output.push_str(&hint);
                    }
                } else {
                    unproductive_streak = 0;
                    self.tool_failure_guard.record_success(name);
                    // Feed the broker's learned re-rank: a revealed tool that ran
                    // successfully ranks higher next time (no-op when learning off
                    // or the tool was not revealed).
                    if let Some(broker) = &self.broker {
                        broker.note_success(name);
                    }
                    self.error_breaker.reset();
                    // No-progress breaker: a successful call that keeps repeating
                    // with the same result, or a turn cycling a tiny set of calls,
                    // gets one strategy-change nudge before the budget controller
                    // may stop the turn. The signature pairs the tool with its
                    // arguments; the output is the observable state, so a re-read
                    // after a real change (different output) is not flagged.
                    let signature = format!("{name}\u{1f}{input}");
                    if no_progress.observe(&signature, &result.output) {
                        let _ = events.send(RuntimeEvent::Warning(
                            "tool calls are not making forward progress; nudging a strategy change"
                                .to_string(),
                        ));
                        result.output.push_str(&no_progress_hint());
                    }
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
                // Verifier stage: judge the call against its contract and record
                // the verdict durably, so a later claim can be checked against it.
                let verdict = self.verify_call(name, input, &result);
                self.record_event(SessionEventKind::ToolVerified {
                    id: result.id.to_string(),
                    verdict,
                });
                self.append(Message::new(
                    Role::Tool,
                    vec![ContentBlock::ToolResult(result)],
                ));
            }
        }
    }

    /// The handoff for the most recently finished turn, if any — a bounded,
    /// parseable terminal summary (stop reason, tool calls, files changed, whether
    /// memory was written) a non-interactive caller can read after `run_turn`.
    #[must_use]
    pub fn last_turn_handoff(&self) -> Option<&TurnHandoff> {
        self.last_handoff.as_ref()
    }

    fn stop(&mut self, events: &broadcast::Sender<RuntimeEvent>, reason: StopReason) -> StopReason {
        // The turn has ended: a switch is allowed again at this boundary.
        self.turn_in_flight = false;
        // Build the per-turn handoff from the state tracked across the turn, so a
        // non-interactive caller always has a terminal summary to read — even when
        // the turn timed out or was cut off. The durable record stays the event log.
        self.last_handoff = Some(TurnHandoff {
            reason,
            tool_calls: self.turn_tool_calls,
            files_changed: self.turn_files_changed.clone(),
            memory_written: self.turn_memory_written,
        });
        if reason == StopReason::Cancelled {
            self.record_event(SessionEventKind::Cancelled);
        }
        self.record_event(SessionEventKind::TurnEnded {
            stop: format!("{reason:?}"),
        });
        // Best-effort, post-turn usage tracking: bump the hit count of every
        // memory this turn injected. Delivered once here at the single turn-exit
        // (so it covers interactive and headless alike) and never on the
        // retrieval read path, keeping retrieval read-only and fast.
        self.hooks.record_usage(&self.turn_memories_used);
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

/// Replace raw control characters (other than tab, newline, carriage return)
/// with a printable `\xNN` escape before tool output enters the model context.
///
/// Output captured via `String::from_utf8_lossy` keeps NUL and other C0/C1
/// control bytes verbatim — only invalid UTF-8 becomes `U+FFFD`. A tool that
/// prints raw binary (e.g. a JSON parser error echoing the bytes of a `.glb`
/// file: `glTF\x02\x00\x00\x00...`) thus injects control characters straight
/// into the prompt. Some local models degenerate when those reach the context,
/// looping on a fragment of the input ("...is not valid JSON") instead of
/// answering. Escaping preserves the visible meaning while removing the trigger.
///
/// Returns `None` when nothing needs escaping, so the common case allocates
/// nothing.
fn scrub_control_chars(text: &str) -> Option<String> {
    if !text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
    {
        return None;
    }
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        if matches!(c, '\t' | '\n' | '\r') {
            out.push(c);
        } else if c.is_control() {
            use std::fmt::Write as _;
            let _ = write!(out, "\\x{:02x}", c as u32);
        } else {
            out.push(c);
        }
    }
    Some(out)
}

fn trim_blank_boundary_lines(mut text: String) -> String {
    let trimmed = text.trim_matches(['\r', '\n']);
    if trimmed.len() != text.len() {
        text = trimmed.to_string();
    }
    text
}

fn invalid_tool_calls(
    calls: &[(String, String, serde_json::Value, Option<serde_json::Value>)],
) -> Option<String> {
    for (id, name, input, _) in calls {
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
    // A stream-decode failure or a malformed tool-argument payload is a bad turn
    // the recovery ladder handles, not a terminal provider error.
    !matches!(
        err,
        ProviderError::StreamDecode(_) | ProviderError::MalformedToolArguments { .. }
    )
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
    use localpilot_llm::{FakeProvider, ProviderDeclaration};
    use localpilot_recovery::RecoveryBudget;
    use localpilot_sandbox::{ScriptedApprover, Workspace};

    /// A fake provider that reports `id` as its declaration id.
    fn fake_with_id(id: &str) -> Arc<dyn ModelProvider> {
        let mut declaration: ProviderDeclaration = FakeProvider::new().declaration().clone();
        declaration.id = id.to_string();
        declaration.display_name = id.to_string();
        Arc::new(FakeProvider::new().with_declaration(declaration))
    }

    /// A runtime seeded with provider `a` and a registry holding `a` + `b`, where
    /// only `b` carries a configured default model. Returns the runtime and its
    /// temp dir (kept alive for the store).
    fn switchable_runtime() -> (SessionRuntime, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
        providers.insert("a".to_string(), fake_with_id("a"));
        providers.insert("b".to_string(), fake_with_id("b"));
        let mut default_models = HashMap::new();
        default_models.insert("b".to_string(), "model-b".to_string());
        let registry = Arc::new(ProviderRegistry::from_providers(
            providers,
            default_models,
            "a",
        ));

        let mut runtime = SessionRuntime::new(
            registry.get("a").unwrap().clone(),
            ToolRegistry::with_builtins(),
            PermissionEngine::new(Profile::Default, Vec::new()),
            Box::new(ScriptedApprover::always()),
            Store::open(dir.path()),
            Workspace::new(dir.path()).unwrap(),
            RecoveryEngine::new(RecoveryBudget::default()),
            SessionConfig {
                model: "model-a".to_string(),
                ..SessionConfig::default()
            },
            Vec::new(),
        );
        runtime.set_registry(registry);
        (runtime, dir)
    }

    #[tokio::test]
    async fn a_profile_swap_through_the_shared_handle_governs_the_next_dispatch() {
        // What a mid-turn `/unrestricted` does: swap the engine through the
        // shared handle while holding no mutable borrow of the runtime, and
        // the next dispatch is evaluated under the new profile.
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = SessionRuntime::new(
            fake_with_id("a"),
            ToolRegistry::with_builtins(),
            PermissionEngine::new(Profile::Default, Vec::new()),
            Box::new(ScriptedApprover::new(Vec::new())),
            Store::open(dir.path()),
            Workspace::new(dir.path()).unwrap(),
            RecoveryEngine::new(RecoveryBudget::default()),
            SessionConfig {
                model: "m".to_string(),
                interactivity: Interactivity::NonInteractive,
                ..SessionConfig::default()
            },
            Vec::new(),
        );

        // Default profile, non-interactive: an unknown command class is denied
        // by the permission engine before it ever spawns.
        let denied = runtime
            .run_user_shell("definitely-not-a-real-command", &[], true)
            .await;
        assert!(
            denied.output.contains("permission denied"),
            "{}",
            denied.output
        );

        let handle = runtime.permission_engine_handle();
        handle.set(PermissionEngine::new(Profile::Unrestricted, Vec::new()));

        // Same call after the swap: the engine no longer denies it (the spawn
        // itself may fail — the command does not exist — but that error is not
        // a permission denial).
        let allowed = runtime
            .run_user_shell("definitely-not-a-real-command", &[], true)
            .await;
        assert!(
            !allowed.output.contains("permission denied"),
            "{}",
            allowed.output
        );
    }

    /// A single-provider runtime over `provider`, for capability checks.
    fn runtime_with_provider(
        provider: Arc<dyn ModelProvider>,
    ) -> (SessionRuntime, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let runtime = SessionRuntime::new(
            provider,
            ToolRegistry::with_builtins(),
            PermissionEngine::new(Profile::Default, Vec::new()),
            Box::new(ScriptedApprover::always()),
            Store::open(dir.path()),
            Workspace::new(dir.path()).unwrap(),
            RecoveryEngine::new(RecoveryBudget::default()),
            SessionConfig {
                model: "m".to_string(),
                ..SessionConfig::default()
            },
            Vec::new(),
        );
        (runtime, dir)
    }

    /// A fake provider whose declaration advertises image input only when asked.
    fn fake_with_vision(vision: bool) -> Arc<dyn ModelProvider> {
        let mut declaration: ProviderDeclaration = FakeProvider::new().declaration().clone();
        if vision
            && !declaration
                .supported_input_blocks
                .contains(&InputBlockKind::Image)
        {
            declaration
                .supported_input_blocks
                .push(InputBlockKind::Image);
        }
        Arc::new(FakeProvider::new().with_declaration(declaration))
    }

    #[test]
    fn active_accepts_images_follows_the_declared_capability() {
        // The image-attach preflight reads exactly this: a declared-vision model
        // passes, a model with no image-input block is refused with guidance.
        let (vision, _d1) = runtime_with_provider(fake_with_vision(true));
        assert!(vision.active_accepts_images());
        let (text_only, _d2) = runtime_with_provider(fake_with_vision(false));
        assert!(!text_only.active_accepts_images());
    }

    #[test]
    fn a_positive_probe_override_lifts_an_undeclared_provider_only_upward() {
        // A probe override adds capability to an undeclared provider...
        let (mut text_only, _d1) = runtime_with_provider(fake_with_vision(false));
        assert!(!text_only.active_accepts_images());
        text_only.set_image_support_override(Some(true));
        assert!(text_only.active_accepts_images());

        // ...but a negative/empty override never removes a declared capability.
        let (mut declared, _d2) = runtime_with_provider(fake_with_vision(true));
        declared.set_image_support_override(Some(false));
        assert!(declared.active_accepts_images());
        declared.set_image_support_override(None);
        assert!(declared.active_accepts_images());
    }

    #[test]
    fn switching_provider_retargets_and_resolves_the_new_default_model() {
        let (mut runtime, _dir) = switchable_runtime();
        assert_eq!(runtime.active_provider_id(), "a");
        assert_eq!(runtime.active_model(), "model-a");
        let history_before = runtime.messages.len();

        let outcome = runtime.set_active_provider("b").unwrap();
        assert_eq!(outcome.provider_id, "b");
        // `b` has a configured default model, so the switch adopts it cleanly.
        assert_eq!(outcome.model, "model-b");
        assert!(outcome.warning.is_none());
        assert_eq!(runtime.active_provider_id(), "b");
        assert_eq!(runtime.active_model(), "model-b");
        // The transcript is provider-neutral and is left untouched by the switch.
        assert_eq!(runtime.messages.len(), history_before);
    }

    #[test]
    fn provider_only_switch_without_a_default_model_keeps_the_current_one() {
        let (mut runtime, _dir) = switchable_runtime();
        // `a` carries no configured default model, so a switch back to it keeps the
        // current model name and surfaces a non-fatal warning rather than failing.
        runtime.set_active_provider("b").unwrap();
        let outcome = runtime.set_active_provider("a").unwrap();
        assert_eq!(outcome.provider_id, "a");
        assert_eq!(outcome.model, "model-b");
        assert!(outcome
            .warning
            .as_deref()
            .is_some_and(|w| w.contains("no configured default model")));
        assert_eq!(runtime.active_model(), "model-b");
    }

    #[test]
    fn switching_to_an_unknown_provider_is_a_typed_error() {
        let (mut runtime, _dir) = switchable_runtime();
        assert_eq!(
            runtime.set_active_provider("nope"),
            Err(SwitchError::UnknownProvider("nope".to_string()))
        );
        // The active provider is unchanged after a refused switch.
        assert_eq!(runtime.active_provider_id(), "a");
    }

    #[test]
    fn a_switch_is_refused_while_a_turn_is_in_flight() {
        let (mut runtime, _dir) = switchable_runtime();
        // Simulate a turn in progress: the switch must defer to the next boundary.
        runtime.turn_in_flight = true;
        assert_eq!(
            runtime.set_active_provider("b"),
            Err(SwitchError::TurnInFlight)
        );
        assert_eq!(
            runtime.set_active_model("x"),
            Err(SwitchError::TurnInFlight)
        );
        // Between turns the same switch succeeds.
        runtime.turn_in_flight = false;
        assert!(runtime.set_active_provider("b").is_ok());
    }

    #[test]
    fn set_active_model_repoints_the_model_only() {
        let (mut runtime, _dir) = switchable_runtime();
        runtime.set_active_model("model-z").unwrap();
        assert_eq!(runtime.active_model(), "model-z");
        // The provider is untouched by a model-only change.
        assert_eq!(runtime.active_provider_id(), "a");
    }

    #[test]
    fn a_switch_without_a_registry_is_an_unknown_provider() {
        // A single-provider session (no registry attached) refuses a switch with a
        // typed error rather than panicking, preserving existing behaviour.
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = SessionRuntime::new(
            fake_with_id("solo"),
            ToolRegistry::with_builtins(),
            PermissionEngine::new(Profile::Default, Vec::new()),
            Box::new(ScriptedApprover::always()),
            Store::open(dir.path()),
            Workspace::new(dir.path()).unwrap(),
            RecoveryEngine::new(RecoveryBudget::default()),
            SessionConfig::default(),
            Vec::new(),
        );
        assert_eq!(
            runtime.set_active_provider("anything"),
            Err(SwitchError::UnknownProvider("anything".to_string()))
        );
    }

    #[test]
    fn effective_limit_derives_from_a_known_window_with_a_reserve() {
        assert_eq!(effective_context_limit(Some(32_768), 24_000), 28_672);
        // The configured global limit is the fallback only.
        assert_eq!(effective_context_limit(None, 24_000), 24_000);
        // A tiny window never collapses below the reserve floor.
        assert_eq!(effective_context_limit(Some(1_024), 24_000), 4_096);
    }

    #[test]
    fn need_markers_parse_only_from_marker_lines() {
        // A bare NEED line, and one behind a list bullet, both parse.
        let needs = parse_need_markers("some thought\nNEED: fetch a web page\n- need: run a query");
        assert_eq!(
            needs,
            vec!["fetch a web page".to_string(), "run a query".to_string()]
        );
        // Prose that merely mentions a capability is not a marker.
        assert!(parse_need_markers("I might need to fetch a page").is_empty());
        // An empty capability after the prefix is ignored.
        assert!(parse_need_markers("NEED:   ").is_empty());
    }

    #[test]
    fn need_markers_are_bounded() {
        let many = "NEED: a\nNEED: b\nNEED: c\nNEED: d\nNEED: e";
        assert_eq!(parse_need_markers(many).len(), 3);
    }

    #[test]
    fn assistant_text_trims_blank_boundary_lines() {
        assert_eq!(
            trim_blank_boundary_lines("\r\n\nThe answer\n\n".to_string()),
            "The answer"
        );
    }

    #[test]
    fn scrub_control_chars_escapes_binary_and_keeps_whitespace() {
        // The exact byte pattern from the .glb parser-error loop.
        let poisoned =
            "Unexpected token 'g', \"glTF\u{02}\u{00}\u{00}\u{00}\"... is not valid JSON";
        let scrubbed = scrub_control_chars(poisoned).expect("control chars present");
        assert_eq!(
            scrubbed,
            "Unexpected token 'g', \"glTF\\x02\\x00\\x00\\x00\"... is not valid JSON"
        );
        assert!(!scrubbed.contains('\u{00}'));
        // Ordinary whitespace is preserved verbatim, and clean text is untouched.
        assert_eq!(scrub_control_chars("line1\n\tline2\r\n"), None);
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
        // After success the counter is gone: the next failure starts again at one.
        assert_eq!(guard.record_failure("edit_file"), 1);
    }

    #[test]
    fn tool_failure_guard_resets_across_turns() {
        let mut guard = ToolFailureGuard::default();
        let mut last = 0;
        for _ in 0..6 {
            last = guard.record_failure("find_files");
        }
        assert_eq!(last, 6);

        // Simulate a new turn boundary.
        guard.reset();
        // After reset the counter starts over.
        assert_eq!(guard.record_failure("find_files"), 1);
    }

    #[test]
    fn tool_failure_guard_independent_per_tool() {
        let mut guard = ToolFailureGuard::default();
        let mut last = 0;
        for _ in 0..5 {
            last = guard.record_failure("tool_a");
        }
        // tool_a is at 5; tool_b is independent and starts at one.
        assert_eq!(last, 5);
        assert_eq!(guard.record_failure("tool_b"), 1);
        assert_eq!(guard.record_failure("tool_b"), 2);
    }
}
