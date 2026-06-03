//! Adapter errors.

/// An error from the LocalMind learning adapter.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LearningError {
    /// The session transcript could not be read from the Unshackled store.
    #[error("read transcript: {0}")]
    Transcript(String),

    /// The project's LocalMind configuration could not be discovered.
    #[error("localmind config: {0}")]
    Config(String),

    /// Importing the transcript into LocalMind failed.
    #[error("localmind import: {0}")]
    Import(String),

    /// The session close-out (summary + candidate extraction) failed.
    #[error("localmind closeout: {0}")]
    Closeout(String),

    /// A review-queue operation failed.
    #[error("localmind review: {0}")]
    Review(String),

    /// A memory operation (promotion, search, audit) failed.
    #[error("localmind memory: {0}")]
    Memory(String),

    /// A context export/retrieval operation failed.
    #[error("localmind context: {0}")]
    Context(String),

    /// A skill-draft operation failed.
    #[error("localmind skill: {0}")]
    Skill(String),
}
