//! Typed errors for patch generation.

use std::path::PathBuf;

use thiserror::Error;

/// Errors raised while proposing or promoting a patch.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PatchError {
    /// A git invocation failed. `args` is the fixed subcommand (never a shell
    /// string); `message` is git's stderr.
    #[error("git {args} failed: {message}")]
    Git { args: String, message: String },

    /// An edit path escaped the worktree root (absolute, `..`, or a drive prefix).
    #[error("edit path escapes the worktree: {0}")]
    OutsideWorktree(String),

    /// An edit targeted a file the finding did not name (out of scope).
    #[error("out-of-scope edit rejected: {0} is not among the finding's files")]
    OutOfScope(String),

    /// The proposal listed no edits, or every edit was a no-op (nothing to change).
    #[error("empty or no-op proposal: nothing to change")]
    EmptyProposal,

    /// The change-provenance record was missing required fields.
    #[error("incomplete change-provenance record (needs prompt, model, rationale, rollback)")]
    IncompleteProvenance,

    /// The worktree diff touched a path outside the finding's named files.
    #[error("the produced change touched an out-of-scope path: {0}")]
    OutOfScopeChange(String),

    /// A branch name was not a safe, simple identifier.
    #[error("invalid branch name: {0}")]
    InvalidBranch(String),

    /// The approval token does not match this patch — promotion refused.
    #[error("approval token does not authorize this patch")]
    TokenMismatch,

    /// Promotion was refused because the target working tree has uncommitted
    /// changes; the human must resolve them first (no clobbering).
    #[error("target working tree is dirty; refusing to promote (resolve local changes first)")]
    DirtyTarget,

    /// Promotion could not fast-forward (the base moved); the human must rebase.
    #[error("cannot fast-forward the proposal onto the current branch; rebase needed")]
    NotFastForward,

    /// A filesystem error at `path`.
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

    /// No proposed patch (worktree + provenance record) was found for the given
    /// id, so it cannot be reopened to promote or discard.
    #[error("no proposed patch found for id: {0}")]
    UnknownProposal(String),

    /// The persisted proposal record could not be (de)serialized.
    #[error("proposal record (de)serialization failed: {0}")]
    Serde(String),
}
