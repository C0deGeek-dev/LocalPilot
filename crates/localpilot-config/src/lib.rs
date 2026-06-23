//! Configuration schema, loading, and redaction for LocalPilot.
//!
//! Owns the config schema, layered loading with deterministic precedence,
//! environment-variable mapping, and the workspace's shared secret-detection /
//! redaction helpers ([`redact`]). Credentials are never stored in config; only
//! the name of the environment variable carrying each is configured, resolved at
//! use into [`localpilot_core::Secret`].
#![forbid(unsafe_code)]

mod context;
pub mod credentials;
mod error;
mod load;
pub mod redact;
mod schema;

pub use context::{
    ContextDiscovery, ContextFile, ContextKind, ContextScope, ProjectContext, DEFAULT_DIR_DEPTH,
    DEFAULT_IMPORT_DEPTH,
};
pub use credentials::{CredentialError, CredentialSource, CredentialStore};
pub use error::ConfigError;
pub use load::{
    credential_store_path, load, project_config_path, prompt_history_path, user_config_path,
    CliOverrides, ConfigPaths,
};
pub use schema::{
    AutoFix, Cadence, CheckConfig, CompactionConfig, CompactionMode, Config, ContextConfig,
    DocsConfig, HarnessConfig, HistoryConfig, HistoryPersistence, IngestConfig, IngestMode,
    LookupPolicy, McpConfig, McpServerConfig, MemoryConfig, Mode, PermissionProfile,
    PermissionsConfig, ProviderConfig, ProviderSelection, QuotaAutoResume, QuotaConfig,
    RuleSeverity, SkillsConfig, StorageConfig, ToolsConfig,
};
