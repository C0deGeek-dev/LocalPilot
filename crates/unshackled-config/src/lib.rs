//! Configuration schema, loading, and redaction for Unshackled.
//!
//! Owns the config schema, layered loading with deterministic precedence,
//! environment-variable mapping, and the workspace's shared secret-detection /
//! redaction helpers ([`redact`]). Credentials are never stored in config; only
//! the name of the environment variable carrying each is configured, resolved at
//! use into [`unshackled_core::Secret`].
#![forbid(unsafe_code)]

mod error;
mod load;
pub mod redact;
mod schema;

pub use error::ConfigError;
pub use load::{load, project_config_path, user_config_path, CliOverrides, ConfigPaths};
pub use schema::{
    Config, HarnessConfig, Mode, PermissionProfile, PermissionsConfig, ProviderConfig,
    ProviderSelection, QuotaAutoResume, QuotaConfig, RuleSeverity,
};
