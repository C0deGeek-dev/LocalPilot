//! The typed internal hook fabric.
//!
//! The Rust analogue of an extension event bus, built so that extensibility
//! *is* the safety model rather than a way around it:
//!
//! - **Observers** are notify-only lifecycle listeners (turn start/end, tool
//!   execution, compaction, recovery, quota transitions).
//! - **Context hooks** may inject system context before a turn — the one
//!   sanctioned "rewrite context" mutation, applied through the same
//!   `seed_system` path a host would use.
//! - **Tool gates** ([`localpilot_tools::ToolGate`]) run *after* the
//!   permission engine inside dispatch and can only block, never grant. The
//!   permission engine is the always-on first link of that chain.
//!
//! Hook code is in-process, compiled-in Rust: trusted by construction.
//! Third-party extension code never loads in-process — it integrates
//! out-of-process over the RPC/ACP protocols or as an MCP server, where the
//! permission engine mediates it like any other tool source (see
//! docs/extending.md).

use std::sync::Arc;

use localpilot_recovery::ModelHealth;
use localpilot_tools::ToolGate;

use crate::session::StopReason;

/// A notify-only lifecycle event delivered to observers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HookEvent {
    /// A provider turn is starting against `model`.
    TurnStarted { model: String },
    /// The turn loop stopped.
    TurnEnded { reason: StopReason },
    /// A tool execution started.
    ToolStarted { id: String, name: String },
    /// A tool execution finished.
    ToolFinished {
        id: String,
        name: String,
        is_error: bool,
    },
    /// Context compaction trimmed history for the next request.
    Compacted,
    /// Recovery recorded a bad turn; current model health attached.
    Recovery { health: ModelHealth },
    /// The provider rate-limited or exhausted quota.
    QuotaPaused { reset: String },
    /// A quality-gate check finished.
    GateCheck { name: String, passed: bool },
}

/// A notify-only lifecycle listener. Observers cannot mutate the session or
/// influence any decision; failures in an observer must be contained by the
/// observer itself.
pub trait SessionObserver: Send + Sync {
    /// A stable name for diagnostics.
    fn name(&self) -> &str;
    /// Receive one lifecycle event.
    fn on_event(&self, event: &HookEvent);
}

/// A pre-turn context hook: may contribute system context for the upcoming
/// turn (the sanctioned context mutation). Returning `None` contributes
/// nothing.
pub trait ContextHook: Send + Sync {
    /// A stable name for diagnostics.
    fn name(&self) -> &str;
    /// Optional system context for a turn that starts with `prompt`.
    fn context_for(&self, prompt: &str) -> Option<String>;
    /// The memories this hook contributed for `prompt`, for the "memories used
    /// this turn" inspector. Default none; a hook that retrieves memory
    /// overrides it. Reporting these never changes what is injected — it only
    /// records what was used.
    fn memories_used(&self, _prompt: &str) -> Vec<localpilot_store::MemoryUsed> {
        Vec::new()
    }
}

/// The registered hooks for one session runtime.
#[derive(Default, Clone)]
pub struct HookFabric {
    observers: Vec<Arc<dyn SessionObserver>>,
    context_hooks: Vec<Arc<dyn ContextHook>>,
    gates: Vec<Arc<dyn ToolGate>>,
}

impl HookFabric {
    /// Register a notify-only observer.
    pub fn register_observer(&mut self, observer: Arc<dyn SessionObserver>) {
        self.observers.push(observer);
    }

    /// Register a pre-turn context hook.
    pub fn register_context_hook(&mut self, hook: Arc<dyn ContextHook>) {
        self.context_hooks.push(hook);
    }

    /// Register a tighten-only tool gate, consulted after the permission
    /// engine on every dispatch.
    pub fn register_gate(&mut self, gate: Arc<dyn ToolGate>) {
        self.gates.push(gate);
    }

    /// Deliver one event to every observer.
    pub(crate) fn notify(&self, event: &HookEvent) {
        for observer in &self.observers {
            observer.on_event(event);
        }
    }

    /// Collect context contributions for a turn.
    pub(crate) fn context_for(&self, prompt: &str) -> Vec<String> {
        self.context_hooks
            .iter()
            .filter_map(|hook| hook.context_for(prompt))
            .collect()
    }

    /// Collect the memories every context hook used for a turn, for the
    /// inspector. Best-effort and side-effect-free.
    pub(crate) fn memories_used(&self, prompt: &str) -> Vec<localpilot_store::MemoryUsed> {
        self.context_hooks
            .iter()
            .flat_map(|hook| hook.memories_used(prompt))
            .collect()
    }

    /// The registered gates, for dispatch.
    pub(crate) fn gates(&self) -> Vec<&dyn ToolGate> {
        self.gates.iter().map(AsRef::as_ref).collect()
    }
}

impl std::fmt::Debug for HookFabric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookFabric")
            .field("observers", &self.observers.len())
            .field("context_hooks", &self.context_hooks.len())
            .field("gates", &self.gates.len())
            .finish()
    }
}
