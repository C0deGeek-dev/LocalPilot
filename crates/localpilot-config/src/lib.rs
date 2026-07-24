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
    credential_store_path, learning_notice_marker_path, load, project_config_path,
    prompt_history_path, user_config_path, CliOverrides, ConfigPaths,
};
pub use schema::{
    AutoFix, Cadence, CheckConfig, CompactionConfig, CompactionMode, Config, ContextConfig,
    DiscoveryConfig, DocsConfig, GuidanceConfig, HarnessConfig, HistoryConfig, HistoryPersistence,
    IngestConfig, IngestMode, LookupPolicy, McpConfig, McpServerConfig, MemoryConfig, Mode,
    PermissionProfile, PermissionsConfig, ProviderAuth, ProviderConfig, ProviderSelection,
    QuotaAutoResume, QuotaConfig, RenderMode, RepairMode, ResearchConfig, ResearchMcpConfig,
    ResearchMcpTool, ResearchRenderConfig, ResearchWebConfig, ResolvedRails, RuleSeverity,
    SelfImprovementConfig, SkillsConfig, StorageConfig, ToolsConfig,
    DEFAULT_HEADLESS_TOOL_BUDGET_MAX, DEFAULT_HEADLESS_TURN_TIMEOUT_SECS,
    DEFAULT_INTERACTIVE_TOOL_BUDGET_MAX,
};
