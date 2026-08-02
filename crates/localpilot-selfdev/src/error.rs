//! Errors from the self-build layer.

/// An error from reading source state, planning or running a build, or
/// publishing the result.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SelfDevError {
    /// A filesystem operation failed.
    #[error("{0}")]
    Io(String),

    /// `git` could not be run, or exited non-zero. Carries the subcommand and
    /// whatever git said, because "git failed" alone is never actionable.
    #[error("git {command} failed: {detail}")]
    Git { command: String, detail: String },

    /// The directory handed over is not inside a git working tree, so there is
    /// no source state to read.
    #[error("{0} is not inside a git working tree")]
    NotAWorkingTree(String),

    /// A build command exited non-zero.
    #[error("build failed ({status}): {detail}")]
    Build { status: String, detail: String },

    /// A caller handed over something inconsistent.
    #[error("invalid: {0}")]
    Invalid(String),
}

impl SelfDevError {
    /// Wrap any displayable I/O failure.
    pub(crate) fn io(error: impl std::fmt::Display) -> Self {
        SelfDevError::Io(error.to_string())
    }
}
