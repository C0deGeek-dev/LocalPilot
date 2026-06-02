//! Configuration error type.

/// Errors produced while resolving or loading configuration.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// A layer failed to load or a value failed to deserialize. The underlying
    /// figment error names the offending key/section.
    #[error("invalid configuration: {0}")]
    Invalid(#[from] figment::Error),

    /// A configuration file could not be read.
    #[error("could not read configuration file {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}
