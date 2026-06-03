//! Configuration loading and precedence.
//!
//! Precedence, highest first: CLI flags, environment variables, the project
//! `.unshackled.toml`, the user config file, then built-in defaults. Credentials
//! are never read from config files — only the *name* of the environment
//! variable holding each is configured, and the value is resolved at use into a
//! [`Secret`].

use std::path::{Path, PathBuf};

use figment::providers::{Env, Format, Serialized, Toml};
use figment::Figment;
use unshackled_core::Secret;

use crate::error::ConfigError;
use crate::schema::{Config, Mode, PermissionProfile};

/// The file locations a load should consider. Either may be `None` (absent).
#[derive(Debug, Clone, Default)]
pub struct ConfigPaths {
    pub user: Option<PathBuf>,
    pub project: Option<PathBuf>,
}

impl ConfigPaths {
    /// Resolve the standard locations: the per-user config file and the project
    /// `.unshackled.toml` under `project_root`.
    #[must_use]
    pub fn standard(project_root: &Path) -> Self {
        Self {
            user: user_config_path(),
            project: Some(project_config_path(project_root)),
        }
    }
}

/// Highest-precedence overrides supplied on the command line. Only set fields
/// override; `None` leaves the lower layers in place.
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub provider_default: Option<String>,
    pub mode: Option<Mode>,
    pub permission_profile: Option<PermissionProfile>,
}

/// Load configuration by layering every source in precedence order.
///
/// # Errors
/// Returns [`ConfigError::Invalid`] if a layer fails to parse or a value has the
/// wrong type; the underlying error names the offending key.
pub fn load(paths: &ConfigPaths, cli: &CliOverrides) -> Result<Config, ConfigError> {
    let mut figment = Figment::from(Serialized::defaults(Config::default()));

    if let Some(user) = &paths.user {
        if user.is_file() {
            figment = figment.merge(Toml::file(user));
        }
    }
    if let Some(project) = &paths.project {
        if project.is_file() {
            figment = figment.merge(Toml::file(project));
        }
    }

    figment = figment.merge(Env::prefixed("UNSHACKLED_").split("__"));

    if let Some(provider) = &cli.provider_default {
        figment = figment.merge(Serialized::default("provider.default", provider));
    }
    if let Some(mode) = &cli.mode {
        figment = figment.merge(Serialized::default("harness.mode", mode));
    }
    if let Some(profile) = &cli.permission_profile {
        figment = figment.merge(Serialized::default("permissions.profile", profile));
    }

    figment.extract().map_err(ConfigError::from)
}

/// The per-user config file location, resolved cross-platform without hardcoded
/// paths. Returns `None` when no suitable base directory is set.
#[must_use]
pub fn user_config_path() -> Option<PathBuf> {
    config_base_dir().map(|base| base.join("unshackled").join("config.toml"))
}

/// The project config file location under `root`.
#[must_use]
pub fn project_config_path(root: &Path) -> PathBuf {
    root.join(".unshackled.toml")
}

#[cfg(windows)]
fn config_base_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(PathBuf::from)
}

#[cfg(not(windows))]
fn config_base_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
}

impl Config {
    /// Resolve the credential for `provider_id` from the environment variable
    /// named by its `api_key_env`, wrapped so it cannot leak through formatting.
    /// Returns `None` when the provider, the env-var name, or its value is absent
    /// or empty.
    #[must_use]
    pub fn resolve_credential(&self, provider_id: &str) -> Option<Secret> {
        let provider = self.providers.get(provider_id)?;
        let env_name = provider
            .api_key_env
            .as_deref()
            .or_else(|| default_api_key_env(&provider.kind))?;
        let value = std::env::var(env_name).ok()?;
        if value.trim().is_empty() {
            None
        } else {
            Some(Secret::new(value))
        }
    }

    /// Resolve the default model for the selected provider (or the configured
    /// default provider when `provider_id` is `None`). Returns `None` when the
    /// provider has no configured model.
    #[must_use]
    pub fn resolve_model(&self, provider_id: Option<&str>) -> Option<String> {
        let id = provider_id.unwrap_or(self.provider.default.as_str());
        let provider = self.providers.get(id)?;
        provider
            .model
            .clone()
            .or_else(|| default_model_env(&provider.kind).and_then(|name| std::env::var(name).ok()))
    }
}

fn default_api_key_env(kind: &str) -> Option<&'static str> {
    match kind {
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openai" | "openai-compatible" | "local" | "custom" | "custom-user-endpoint" => {
            Some("OPENAI_API_KEY")
        }
        _ => None,
    }
}

fn default_model_env(kind: &str) -> Option<&'static str> {
    match kind {
        "anthropic" => Some("ANTHROPIC_MODEL"),
        _ => None,
    }
}
