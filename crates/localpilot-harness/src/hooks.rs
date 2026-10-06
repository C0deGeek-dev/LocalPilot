//! The typed internal hook fabric.
//!
//! Extensibility is part of the safety model rather than a way around it:
//!
//! - **Context hooks** may inject context before a turn — the one sanctioned
//!   "rewrite context" mutation. Each hook says whether its text belongs in
//!   the system prompt or beside the question ([`ContextPlacement`]).
//!
//! Hook code is in-process, compiled-in Rust: trusted by construction.
//! Third-party extension code never loads in-process — it integrates
//! out-of-process over the RPC/ACP protocols or as an MCP server, where the
//! permission engine mediates it like any other tool source (see
//! docs/extending.md).

use std::sync::Arc;

/// Where a context hook's text goes in an ordinary session's request.
///
/// Retrieved context that sits at the end of a long system prompt is passed over
/// by the models measured so far (ADR-0217). Standing orientation that holds
/// whatever the question is belongs in the system prompt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ContextPlacement {
    /// Beside the question it was retrieved for, in the turn's user message.
    #[default]
    BesideQuestion,
    /// In the system prompt: standing instructions and layout facts.
    System,
}

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
    /// Where this hook's text goes when the session places retrieved context
    /// beside the question. Default beside the question, which suits context
    /// retrieved for the prompt; a hook that contributes standing instructions
    /// returns [`ContextPlacement::System`]. With that placement switched off
    /// every hook's text goes in the system prompt, as before.
    fn placement(&self) -> ContextPlacement {
        ContextPlacement::BesideQuestion
    }
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

/// Every hook's contribution for one turn: the text segments in registration
/// order, each with its placement, and the memories they represent.
///
/// Segments stay in order, rather than being split by placement up front, so
/// that placing everything in the system prompt reproduces the request exactly
/// as it was before placement existed.
pub(crate) struct TurnContribution {
    pub(crate) segments: Vec<(ContextPlacement, String)>,
    pub(crate) memories: Vec<localpilot_store::MemoryUsed>,
}

impl TurnContribution {
    /// All segments joined in registration order, wherever they were placed.
    pub(crate) fn all(&self) -> String {
        self.segments
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The segments with one placement, joined in registration order.
    pub(crate) fn placed(&self, placement: ContextPlacement) -> String {
        self.segments
            .iter()
            .filter(|(at, _)| *at == placement)
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The registered hooks for one session runtime.
#[derive(Default, Clone)]
pub struct HookFabric {
    context_hooks: Vec<Arc<dyn ContextHook>>,
}

impl HookFabric {
    /// Register a pre-turn context hook.
    pub fn register_context_hook(&mut self, hook: Arc<dyn ContextHook>) {
        self.context_hooks.push(hook);
    }

    /// Collect every hook's contribution for a turn as one value — the merged
    /// injected text and the exact memories that text represents — in a single
    /// pass, so the audit and the injection are derived from the same retrieval.
    pub(crate) fn contribute(&self, prompt: &str) -> TurnContribution {
        let mut segments = Vec::new();
        let mut memories = Vec::new();
        for hook in &self.context_hooks {
            let contribution = hook.contribute(prompt);
            if let Some(text) = contribution.text {
                segments.push((hook.placement(), text));
            }
            memories.extend(contribution.memories);
        }
        TurnContribution { segments, memories }
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
}

impl std::fmt::Debug for HookFabric {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookFabric")
            .field("context_hooks", &self.context_hooks.len())
            .finish()
    }
}
