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
/// What a context hook contributes for one turn: the system-context text that is
/// injected, and the exact memory records that text represents (for the
/// "memories used" inspector). Deriving both from one value is what keeps the
/// audit equal to the injection — the audit can never list a memory the turn did
/// not actually inject, nor omit one it did.
#[derive(Default)]
pub struct ContextContribution {
    /// The system-context text injected for the turn, or `None` to inject
    /// nothing.
    pub text: Option<String>,
    /// The memory records the injected text represents, in injection order.
    pub memories: Vec<localpilot_store::MemoryUsed>,
}

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
    /// The injected text *and* the exact memories it represents, as one value so
    /// the injection and the audit cannot diverge. The default derives from
    /// [`ContextHook::context_for`]/[`ContextHook::memories_used`]; a hook that
    /// retrieves memory overrides this to compute both from a single retrieval.
    fn contribute(&self, prompt: &str) -> ContextContribution {
        ContextContribution {
            text: self.context_for(prompt),
            memories: self.memories_used(prompt),
        }
    }
    /// Record that `memories` were injected this turn, for usage tracking. Called
    /// once **post-turn** (from the single turn-exit), never on the retrieval
    /// read path, so a usage write cannot slow a turn. Default does nothing; a
    /// hook backed by a store overrides it to bump per-memory hit counts
    /// best-effort. A failure here must never fail the turn.
    fn record_usage(&self, _memories: &[localpilot_store::MemoryUsed]) {}
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

    /// Collect every hook's contribution for a turn as one value — the merged
    /// injected text and the exact memories that text represents — in a single
    /// pass, so the audit and the injection are derived from the same retrieval.
    pub(crate) fn contribute(&self, prompt: &str) -> ContextContribution {
        let mut texts = Vec::new();
        let mut memories = Vec::new();
        for hook in &self.context_hooks {
            let contribution = hook.contribute(prompt);
            if let Some(text) = contribution.text {
                texts.push(text);
            }
            memories.extend(contribution.memories);
        }
        ContextContribution {
            text: (!texts.is_empty()).then(|| texts.join("\n")),
            memories,
        }
    }

    /// Deliver this turn's injected-memory set to every context hook for usage
    /// tracking. Called once at the turn boundary (post-turn), so the bump never
    /// rides the retrieval read path.
    pub(crate) fn record_usage(&self, memories: &[localpilot_store::MemoryUsed]) {
        if memories.is_empty() {
            return;
        }
        for hook in &self.context_hooks {
            hook.record_usage(memories);
        }
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
