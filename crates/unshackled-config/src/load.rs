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
use crate::schema::{CheckConfig, Config, Mode, PermissionProfile};

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

    let mut config: Config = figment.extract().map_err(ConfigError::from)?;
    synthesize_env_providers(&mut config);
    validate_checks(&config.harness.checks)?;
    Ok(config)
}

/// Validate the ratified quality-gate checks: each needs a non-empty name and
/// program, and names must be unique (a name is also a per-check override key).
fn validate_checks(checks: &[CheckConfig]) -> Result<(), ConfigError> {
    let mut seen = std::collections::HashSet::new();
    for check in checks {
        if check.name.trim().is_empty() {
            return Err(ConfigError::InvalidCheck(
                "a check has an empty name".to_string(),
            ));
        }
        if check.program.trim().is_empty() {
            return Err(ConfigError::InvalidCheck(format!(
                "check {:?} has an empty program",
                check.name
            )));
        }
        if !seen.insert(check.name.as_str()) {
            return Err(ConfigError::InvalidCheck(format!(
                "duplicate check name {:?}",
                check.name
            )));
        }
    }
    Ok(())
}

/// When no providers are configured, derive a default one from the documented
/// public provider env vars so a launcher that exports them (e.g.
/// `ANTHROPIC_BASE_URL`) works with no config file. Anthropic is preferred when
/// both are present. Existing configured providers are never overridden; the
/// registry fills their missing base URLs from the same env vars.
fn synthesize_env_providers(config: &mut Config) {
    use crate::schema::ProviderConfig;

    if !config.providers.is_empty() {
        return;
    }

    // Register the env-derived provider under the existing default id so the
    // configured `[provider].default` (or the built-in) keeps pointing at it;
    // `provider.default` is never overridden. Anthropic is preferred.
    let id = config.provider.default.clone();
    let synthesized = if let Some(base) = env_non_empty("ANTHROPIC_BASE_URL") {
        Some(ProviderConfig {
            kind: "anthropic".to_string(),
            base_url: Some(base),
            model: env_non_empty("ANTHROPIC_MODEL"),
            ..ProviderConfig::default()
        })
    } else {
        env_non_empty("OPENAI_BASE_URL").map(|base| ProviderConfig {
            kind: "openai-compatible".to_string(),
            base_url: Some(base),
            model: env_non_empty("OPENAI_MODEL"),
            ..ProviderConfig::default()
        })
    };
    if let Some(provider) = synthesized {
        config.providers.insert(id, provider);
    }
}

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
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
        // Try the explicitly named env var first, then the kind's conventional
        // ones in order (Anthropic's gateway auth is carried by
        // `ANTHROPIC_AUTH_TOKEN` when `ANTHROPIC_API_KEY` is empty).
        let candidates = provider
            .api_key_env
            .as_deref()
            .into_iter()
            .chain(default_api_key_envs(&provider.kind).iter().copied());
        for env_name in candidates {
            if let Ok(value) = std::env::var(env_name) {
                if !value.trim().is_empty() {
                    return Some(Secret::new(value));
                }
            }
        }
        None
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

fn default_api_key_envs(kind: &str) -> &'static [&'static str] {
    match kind {
        // Anthropic gateways carry auth in `ANTHROPIC_AUTH_TOKEN` when
        // `ANTHROPIC_API_KEY` is empty; try both.
        "anthropic" => &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"],
        "openai" | "openai-compatible" | "local" | "custom" | "custom-user-endpoint" => {
            &["OPENAI_API_KEY"]
        }
        _ => &[],
    }
}

fn default_model_env(kind: &str) -> Option<&'static str> {
    match kind {
        "anthropic" => Some("ANTHROPIC_MODEL"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{AutoFix, Cadence};

    fn check(name: &str, program: &str) -> CheckConfig {
        CheckConfig {
            name: name.to_string(),
            program: program.to_string(),
            args: Vec::new(),
            fix_program: None,
            fix_args: Vec::new(),
            cadence: Cadence::Step,
            auto_fix: AutoFix::No,
            severity: None,
        }
    }

    #[test]
    fn validate_accepts_unique_named_checks() {
        let checks = [check("fmt", "cargo"), check("clippy", "cargo")];
        assert!(validate_checks(&checks).is_ok());
    }

    #[test]
    fn validate_rejects_duplicate_names() {
        let checks = [check("fmt", "cargo"), check("fmt", "cargo")];
        let err = validate_checks(&checks).expect_err("duplicate name should fail");
        assert!(err.to_string().contains("duplicate check name"));
    }

    #[test]
    fn validate_rejects_empty_name_or_program() {
        assert!(validate_checks(&[check("", "cargo")]).is_err());
        assert!(validate_checks(&[check("fmt", "  ")]).is_err());
    }
}
