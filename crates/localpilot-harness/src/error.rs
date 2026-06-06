//! Harness error type.

/// Errors from parsing project files or running the harness.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HarnessError {
    /// A required section was missing from a project document.
    #[error("{document} is missing the required section: {section}")]
    MissingSection {
        document: &'static str,
        section: String,
    },

    /// A project document was malformed at a specific place.
    #[error("{document} is malformed: {detail}")]
    Malformed {
        document: &'static str,
        detail: String,
    },

    /// A filesystem operation failed.
    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// The provider failed or did not produce a usable document after retries.
    #[error("provider error: {0}")]
    Provider(String),
}
