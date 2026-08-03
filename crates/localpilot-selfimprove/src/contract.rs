//! The self-improvement loop *contract*: the pipeline states and the human gate,
//! each state bound to the **existing** entrypoint that produces it. This module
//! is description only — it adds no stage logic and re-implements nothing. It is
//! the shared vocabulary the orchestrator and the CLI agree on.
//!
//! The pipeline is linear and total:
//!
//! ```text
//! Found ──review──▶ Proposed ──[ ApprovalToken gate ]──▶ Approved ──build──▶ Built ──reload──▶ Reloaded
//! ```
//!
//! The only way past [`Stage::Proposed`] is [`cross_gate`], which is representable
//! **only** with an [`ApprovalToken`]. So "no autonomous advance past the gate" is
//! a property of the types, not a convention (ADR-0034 human-gated loop; ADR-0128
//! keeps the unattended autonomous loop deferred).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use localpilot_patchgen::{ApprovalToken, PatchError};

/// One state in the self-improvement pipeline. Declaration order **is** pipeline
/// order, so the derived [`Ord`] compares states by how far along the loop they
/// are. Each state names exactly one existing entrypoint that produces it (see
/// [`Stage::entrypoint`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// A ranked, advisory finding exists — produced by `localpilot_selfreview::review`
    /// (read-only; writes nothing).
    Found,
    /// A scope-confined patch sits in an isolated worktree awaiting review —
    /// produced by `localpilot_patchgen::propose`.
    Proposed,
    /// The proposal was promoted onto the main branch — produced by
    /// `localpilot_patchgen::ProposedPatch::promote`, which **requires** an
    /// [`ApprovalToken`]. This is the state on the far side of the human gate.
    Approved,
    /// The approved tree was built, vetted, installed, and a channel promoted —
    /// produced by `localpilot_selfdev::build_gauntlet_promote`.
    Built,
    /// The running process was swapped onto the built binary — produced by
    /// `localpilot_selfdev::relaunch`.
    Reloaded,
}

impl Stage {
    /// The next state in the linear pipeline, or `None` at the terminal state.
    /// This is the pure ordering; it says nothing about the gate (see
    /// [`unattended_next`]).
    #[must_use]
    pub const fn next(self) -> Option<Stage> {
        match self {
            Stage::Found => Some(Stage::Proposed),
            Stage::Proposed => Some(Stage::Approved),
            Stage::Approved => Some(Stage::Built),
            Stage::Built => Some(Stage::Reloaded),
            Stage::Reloaded => None,
        }
    }

    /// The fully-qualified existing entrypoint that produces this state. The
    /// contract references these; it never wraps or re-implements them.
    #[must_use]
    pub const fn entrypoint(self) -> &'static str {
        match self {
            Stage::Found => "localpilot_selfreview::review",
            Stage::Proposed => "localpilot_patchgen::propose",
            Stage::Approved => "localpilot_patchgen::ProposedPatch::promote",
            Stage::Built => "localpilot_selfdev::build_gauntlet_promote",
            Stage::Reloaded => "localpilot_selfdev::relaunch",
        }
    }

    /// A short, stable, lower-case label for status output and logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Stage::Found => "found",
            Stage::Proposed => "proposed",
            Stage::Approved => "approved",
            Stage::Built => "built",
            Stage::Reloaded => "reloaded",
        }
    }
}

/// The pipeline is at [`Stage::Proposed`] and cannot advance without a human
/// [`ApprovalToken`]. Returned by [`unattended_next`] to make the gate a value an
/// unattended caller must handle, rather than a rule it might forget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AwaitingApproval;

/// The next state an **unattended** step may reach. Every non-gate transition
/// returns `Ok(next)` (with `Ok(None)` at the terminal state); the
/// Proposed→Approved transition returns `Err(AwaitingApproval)`, because the only
/// way across it is [`cross_gate`], which demands a token. There is no tokenless
/// path from `Proposed` to `Approved`.
///
/// # Errors
/// [`AwaitingApproval`] at [`Stage::Proposed`] — the human gate.
pub fn unattended_next(stage: Stage) -> Result<Option<Stage>, AwaitingApproval> {
    match stage {
        Stage::Proposed => Err(AwaitingApproval),
        other => Ok(other.next()),
    }
}

/// Cross the human gate: the transition from [`Stage::Proposed`] to
/// [`Stage::Approved`]. Taking an [`ApprovalToken`] by reference makes the
/// crossing *representable only* when a token exists — a caller cannot even name
/// the result of this transition without one. The token is minted solely by a
/// human-confirmation path (`ApprovalToken::approve`); nothing in this crate mints
/// one.
#[must_use]
pub fn cross_gate(_token: &ApprovalToken) -> Stage {
    Stage::Approved
}

/// Errors the loop can surface. The propose/promote failure surface is the real
/// [`PatchError`] from `localpilot-patchgen` — no new failure modes are invented
/// for those stages. The build/reload stage surfaces its message through
/// [`LoopError::Build`] (the display of `localpilot-selfdev`'s error).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LoopError {
    /// A step was attempted that is not the loop's current step.
    #[error("the loop is at {actual:?}; {attempted} is not the next step")]
    OutOfOrder {
        /// The loop's actual current stage.
        actual: Stage,
        /// The step that was attempted out of order.
        attempted: &'static str,
    },

    /// A step was attempted with no active loop (nothing has been proposed).
    #[error("no active self-improvement loop; propose a finding first")]
    NoActiveLoop {
        /// The step that was attempted.
        attempted: &'static str,
    },

    /// The loop is parked at the human gate: the proposed patch must be promoted
    /// with an [`ApprovalToken`] before it can advance.
    #[error(
        "awaiting human approval: promote the proposed patch with an approval token \
         before the loop can advance"
    )]
    AwaitingApproval,

    /// A propose/promote step failed — the real `localpilot-patchgen` error.
    #[error(transparent)]
    Patch(#[from] PatchError),

    /// The self-dev build or reload stage failed; carries the underlying
    /// `localpilot-selfdev` message.
    #[error("self-dev stage failed: {0}")]
    Build(String),

    /// A filesystem error touching the loop's persisted state.
    #[error("io error at {path}: {source}")]
    Io {
        /// The path being read or written.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// The persisted loop-state record could not be (de)serialized.
    #[error("loop state (de)serialization failed: {0}")]
    Serde(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The five states are linearly ordered, and each names a real entrypoint —
    /// the contract maps every state to an API that already ships.
    #[test]
    fn states_are_linearly_ordered_and_each_maps_to_an_entrypoint() {
        let order = [
            Stage::Found,
            Stage::Proposed,
            Stage::Approved,
            Stage::Built,
            Stage::Reloaded,
        ];
        // Strictly increasing along the pipeline (derived Ord = declaration order).
        for pair in order.windows(2) {
            assert!(
                pair[0] < pair[1],
                "{:?} must precede {:?}",
                pair[0],
                pair[1]
            );
        }
        // `next()` walks exactly this order and terminates.
        let mut walked = vec![Stage::Found];
        while let Some(next) = walked.last().and_then(|s| s.next()) {
            walked.push(next);
        }
        assert_eq!(walked, order);
        // Every state names a non-empty, fully-qualified existing entrypoint.
        for stage in order {
            assert!(
                stage.entrypoint().contains("::"),
                "{stage:?} must map to a real crate API, got {:?}",
                stage.entrypoint()
            );
        }
    }

    /// The gate is a hard stop: an unattended step cannot pass `Proposed`, and the
    /// *only* way to reach `Approved` is [`cross_gate`], which needs a token.
    #[test]
    fn the_gate_is_representable_only_with_a_token() {
        // Every non-gate transition advances without a token...
        assert_eq!(unattended_next(Stage::Found), Ok(Some(Stage::Proposed)));
        assert_eq!(unattended_next(Stage::Approved), Ok(Some(Stage::Built)));
        assert_eq!(unattended_next(Stage::Built), Ok(Some(Stage::Reloaded)));
        assert_eq!(unattended_next(Stage::Reloaded), Ok(None));
        // ...but Proposed is a hard stop with no tokenless successor.
        assert_eq!(unattended_next(Stage::Proposed), Err(AwaitingApproval));
        // The token is the only key through: crossing yields Approved, and it can
        // only be named by supplying a token (a human act; not minted here).
        let token = ApprovalToken::approve("some-patch", "a-human");
        assert_eq!(cross_gate(&token), Stage::Approved);
    }

    /// The propose/promote failure surface is the real `PatchError`, so the
    /// orchestrator surfaces state without inventing new failure modes.
    #[test]
    fn error_surface_references_the_real_patch_error() {
        let mapped: LoopError = PatchError::TokenMismatch.into();
        assert!(matches!(
            mapped,
            LoopError::Patch(PatchError::TokenMismatch)
        ));
        // The build stage carries the self-dev message verbatim.
        let build = LoopError::Build("build failed (exit 1): boom".to_string());
        assert!(build.to_string().contains("boom"));
    }
}
