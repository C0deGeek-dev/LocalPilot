//! Static discipline metadata for a tool: the contract a verifier and a learning
//! loop read to know a tool's side effects, reversibility, pre/postconditions,
//! and how to prove it did what it claimed.
//!
//! The contract is **additive and advisory**. It is pure data — it carries no
//! reference to the permission engine, the evidence ledger, or the verifier, so
//! this crate gains no new dependency. Evaluating a precondition or a
//! postcondition against live session state happens in the caller (the harness /
//! verifier), not here. A tool that returns the default [`ToolContract`] behaves
//! exactly as before; the permission path ([`super::Tool::effects`]) is
//! untouched.
//!
//! The enums are `#[non_exhaustive]` so later work can add variants (e.g. a
//! model-critic verification method, a custom predicate) without breaking
//! callers.

use serde_json::Value;

/// A tool's contract version. Bumping it invalidates version-pinned lessons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolVersion(pub u32);

impl Default for ToolVersion {
    fn default() -> Self {
        Self(1)
    }
}

/// The broad side-effect class a tool falls into. Advisory metadata only — the
/// permission engine still authorizes each concrete effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SideEffectClass {
    /// Reads state, changes nothing.
    #[default]
    ReadOnly,
    /// Writes inside the workspace.
    ProjectWrite,
    /// Writes outside the workspace or to durable external state.
    ExternalWrite,
    /// Talks to the network.
    Network,
    /// Can destroy data that is hard or impossible to recover.
    Destructive,
}

/// Whether a tool's effect can be undone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Reversibility {
    /// Trivially undoable (or no effect).
    Reversible,
    /// Undoable only via a saved artifact (backup, VCS, undo log).
    ReversibleWithArtifact,
    /// Cannot be undone.
    Irreversible,
    /// Not declared.
    #[default]
    Unknown,
}

/// Whether repeating a call with the same arguments has the same effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Idempotency {
    Idempotent,
    NonIdempotent,
    #[default]
    Unknown,
}

/// A workspace state a precondition can require.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StatePredicate {
    /// The working tree has no uncommitted changes.
    CleanWorktree,
    /// The path named by this input field already exists.
    FileExists { path_arg: &'static str },
}

/// A condition the runtime evaluates BEFORE permission. Data only — the caller
/// owns the session evidence it is checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Precondition {
    /// The path in `path_arg` must have been read this session before this call.
    RequiresPriorRead { path_arg: &'static str },
    /// A named workspace state must hold.
    State(StatePredicate),
}

/// What a path-effect postcondition expects of a path after the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PathEffectKind {
    Exists,
    Modified,
    Deleted,
}

/// What a read-back postcondition expects the content to satisfy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ContentExpectation {
    /// The file is non-empty.
    NonEmpty,
    /// The content contains this substring.
    Contains(&'static str),
}

/// A condition the verifier evaluates AFTER execution. Data only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Postcondition {
    /// The path named by `path_arg` was affected as `kind` intends.
    PathEffect {
        path_arg: &'static str,
        kind: PathEffectKind,
    },
    /// The tool's own status indicates success (not merely "did not panic").
    ResultStatus,
    /// A follow-up read of `path_arg` confirms the intended content.
    ConfirmRead {
        path_arg: &'static str,
        expect: ContentExpectation,
    },
}

/// An observed-error → recovery-hint pair, for guided recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureMode {
    /// A substring identifying the observed failure.
    pub observed: &'static str,
    /// A short, model-facing recovery hint.
    pub hint: &'static str,
}

/// Whether and how a failed call may be retried automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum RetryPolicy {
    /// Do not retry automatically.
    #[default]
    None,
    /// Retry up to `max` times with exponential backoff from `base_ms`.
    BoundedBackoff { max: u32, base_ms: u64 },
    /// Never retry automatically (a human/model must decide).
    NeverAutomatic,
}

/// Advisory confirmation strength. The permission engine still decides; this
/// only ever tightens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Confirmation {
    /// No advisory confirmation beyond the permission engine.
    #[default]
    None,
    /// Always advise confirmation.
    Always,
    /// Advise confirmation when the call is above the risk floor.
    AboveRiskFloor,
}

/// How a tool's postcondition is proven.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum VerificationMethod {
    /// The postcondition list is sufficient and deterministic.
    Postconditions,
    /// Re-run a cheap read tool to confirm (no new permission surface).
    ReadBack { tool: &'static str },
    /// Defer to an optional model critic, only if configured.
    ModelCritic,
    /// The effect cannot be verified cheaply; record as unverified, never as a
    /// success.
    #[default]
    Unverifiable,
}

/// A few-shot example of a tool call, retrievable for guidance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolExample {
    /// A representative input, as a JSON string.
    pub input: &'static str,
    /// Why this is a good (or, for a counterexample, bad) call.
    pub note: &'static str,
}

/// Static discipline metadata for a tool. The default is an empty contract, so
/// existing tools need no change.
#[derive(Debug, Clone, Default)]
pub struct ToolContract {
    /// Bumping invalidates version-pinned lessons.
    pub version: ToolVersion,
    /// Model-facing description (may differ from the human `description`).
    pub model_description: &'static str,
    pub side_effect: SideEffectClass,
    pub reversibility: Reversibility,
    pub idempotency: Idempotency,
    pub preconditions: &'static [Precondition],
    pub postconditions: &'static [Postcondition],
    /// Observed-error → recovery-hint pairs.
    pub failure_modes: &'static [FailureMode],
    pub retry: RetryPolicy,
    pub confirmation: Confirmation,
    pub verification: VerificationMethod,
    /// Sibling or replacement tool names.
    pub related: &'static [&'static str],
    /// Positive few-shot examples.
    pub examples: &'static [ToolExample],
    /// Negative few-shot examples (wrong tool / bad args).
    pub counterexamples: &'static [ToolExample],
}

impl ToolContract {
    /// Whether the contract declares any observable side effect.
    #[must_use]
    pub fn has_side_effect(&self) -> bool {
        !matches!(self.side_effect, SideEffectClass::ReadOnly)
    }

    /// Whether the contract gives the verifier something to check: a
    /// postcondition, or an explicit `Unverifiable` admission.
    #[must_use]
    pub fn is_verification_declared(&self) -> bool {
        !self.postconditions.is_empty()
            || matches!(self.verification, VerificationMethod::Unverifiable)
    }
}

/// Extract a string argument from a tool input object, for precondition and
/// postcondition evaluation by a caller that holds the input.
#[must_use]
pub fn string_arg<'a>(input: &'a Value, field: &str) -> Option<&'a str> {
    input.get(field).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_contract_is_empty_and_read_only() {
        let c = ToolContract::default();
        assert_eq!(c.version, ToolVersion(1));
        assert_eq!(c.side_effect, SideEffectClass::ReadOnly);
        assert_eq!(c.reversibility, Reversibility::Unknown);
        assert_eq!(c.idempotency, Idempotency::Unknown);
        assert!(c.preconditions.is_empty());
        assert!(c.postconditions.is_empty());
        assert!(!c.has_side_effect());
        // An empty contract counts as "verification declared" only via the
        // Unverifiable default — it never silently claims success.
        assert_eq!(c.verification, VerificationMethod::Unverifiable);
        assert!(c.is_verification_declared());
    }

    #[test]
    fn a_side_effect_contract_is_flagged() {
        let c = ToolContract {
            side_effect: SideEffectClass::ProjectWrite,
            ..ToolContract::default()
        };
        assert!(c.has_side_effect());
    }

    #[test]
    fn string_arg_reads_object_fields() {
        let input = serde_json::json!({ "path": "src/lib.rs" });
        assert_eq!(string_arg(&input, "path"), Some("src/lib.rs"));
        assert_eq!(string_arg(&input, "missing"), None);
    }
}
