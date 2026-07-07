//! Configuration loading and precedence.
//!
//! Precedence, highest first: CLI flags, environment variables, the project
//! `.localpilot.toml`, the user config file, then built-in defaults. Credentials
//! are never read from config files — only the *name* of the environment
//! variable holding each is configured, and the value is resolved at use into a
//! [`Secret`].

use std::path::{Path, PathBuf};

use figment::providers::{Env, Format, Serialized, Toml};
use figment::Figment;
use localpilot_core::Secret;

use crate::error::ConfigError;
use crate::schema::{CheckConfig, Config, Mode, PermissionProfile, ProviderAuth};

/// The file locations a load should consider. Either may be `None` (absent).
#[derive(Debug, Clone, Default)]
pub struct ConfigPaths {
    pub user: Option<PathBuf>,
    pub project: Option<PathBuf>,
}

impl ConfigPaths {
    /// Resolve the standard locations: the per-user config file and the project
    /// `.localpilot.toml` under `project_root`.
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

    figment = figment.merge(Env::prefixed("LOCALPILOT_").split("__"));

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
        if check.severity == Some(crate::schema::RuleSeverity::Discard) {
            // The per-check severity rides the shared check-runner contract,
            // which has no discard notion; discard is a rule-level escalation.
            return Err(ConfigError::InvalidCheck(format!(
                "check {:?}: severity \"discard\" is rule-level only — set \
                 [harness.rules] (e.g. quality_gate = \"discard\") instead",
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
    config_base_dir().map(|base| base.join("localpilot").join("config.toml"))
}

/// The project config file location under `root`.
#[must_use]
pub fn project_config_path(root: &Path) -> PathBuf {
    root.join(".localpilot.toml")
}

/// The per-user prompt-history store location, resolved cross-platform without
/// hardcoded paths and alongside the user config file (`%APPDATA%/localpilot` on
/// Windows, `$XDG_CONFIG_HOME`/`~/.config/localpilot` elsewhere). Returns `None`
/// when no suitable base directory is set. The store is global and tagged with
/// each prompt's originating directory; recall filters it to the current project.
#[must_use]
pub fn prompt_history_path() -> Option<PathBuf> {
    config_base_dir().map(|base| base.join("localpilot").join("prompt-history.jsonl"))
}

/// The per-user credential fallback-file location, resolved cross-platform
/// alongside the user config file. Holds API keys stored by `login` when the OS
/// keychain is unavailable; owner-only on unix. Returns `None` when no suitable
/// base directory is set.
#[must_use]
pub fn credential_store_path() -> Option<PathBuf> {
    config_base_dir().map(|base| base.join("localpilot").join("credentials.json"))
}

/// Marker file recording that the one-time "learning is on by default" notice has
/// been shown. Resolved per-user alongside the config file. Its presence
/// suppresses the notice on later runs. Returns `None` when no base dir is set
/// (in which case the notice is simply skipped rather than shown every run).
#[must_use]
pub fn learning_notice_marker_path() -> Option<PathBuf> {
    config_base_dir().map(|base| {
        base.join("localpilot")
            .join(".learning-default-notice-shown")
    })
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
    /// Resolve the credential for `provider_id`, wrapped so it cannot leak through
    /// formatting. Precedence: a stored credential (the OS keychain, or the opt-in
    /// fallback file) wins, then the environment variable named by `api_key_env`
    /// (or a provider-kind default). Returns `None` when no source holds one.
    #[must_use]
    pub fn resolve_credential(&self, provider_id: &str) -> Option<Secret> {
        self.resolve_credential_with(provider_id, &crate::credentials::CredentialStore::user())
    }

    /// [`resolve_credential`](Self::resolve_credential) against an explicit store,
    /// so the precedence is testable without touching the real per-user store.
    #[must_use]
    fn resolve_credential_with(
        &self,
        provider_id: &str,
        store: &crate::credentials::CredentialStore,
    ) -> Option<Secret> {
        let provider = self.providers.get(provider_id)?;
        if provider.auth == ProviderAuth::GoogleAdc {
            return None;
        }
        // 1) A logged-in credential (keychain → fallback file) takes precedence, so
        // a stored key needs no environment variable.
        if let Some(secret) = store.get(provider_id) {
            return Some(secret);
        }
        // 2) Then the explicitly named env var, then the kind's conventional ones
        // in order (Anthropic's gateway auth is carried by `ANTHROPIC_AUTH_TOKEN`
        // when `ANTHROPIC_API_KEY` is empty).
        for env_name in self.credential_env_candidates(provider) {
            if let Ok(value) = std::env::var(env_name) {
                if !value.trim().is_empty() {
                    return Some(Secret::new(value));
                }
            }
        }
        None
    }

    /// The tier a credential for `provider_id` resolves from, without exposing the
    /// value — for `doctor`'s source reporting. Mirrors [`resolve_credential`]'s
    /// precedence: stored (keychain/file) → env → none.
    #[must_use]
    pub fn credential_source(&self, provider_id: &str) -> crate::credentials::CredentialSource {
        use crate::credentials::{CredentialSource, CredentialStore};
        let Some(provider) = self.providers.get(provider_id) else {
            return CredentialSource::None;
        };
        if provider.auth == ProviderAuth::GoogleAdc {
            return if provider
                .google_adc_path
                .as_ref()
                .is_some_and(|path| !path.trim().is_empty())
            {
                CredentialSource::GoogleAdcFile
            } else {
                CredentialSource::GoogleAdc
            };
        }
        if let Some(source) = CredentialStore::user().source(provider_id) {
            return source;
        }
        for env_name in self.credential_env_candidates(provider) {
            if std::env::var(env_name).is_ok_and(|value| !value.trim().is_empty()) {
                return CredentialSource::Env;
            }
        }
        CredentialSource::None
    }

    /// The ordered environment-variable names that may carry `provider`'s
    /// credential: its explicit `api_key_env` first, then the kind's defaults.
    fn credential_env_candidates<'a>(
        &self,
        provider: &'a crate::schema::ProviderConfig,
    ) -> impl Iterator<Item = &'a str> {
        provider
            .api_key_env
            .as_deref()
            .into_iter()
            .chain(default_api_key_envs(&provider.kind).iter().copied())
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

    #[test]
    fn credential_resolution_prefers_a_stored_credential_then_env_then_none() {
        use crate::credentials::CredentialStore;
        use crate::schema::ProviderConfig;

        // A provider whose credential env var is uniquely named for this test, so
        // it never collides with another test's environment.
        const ENV: &str = "LOCALPILOT_TEST_CRED_PRECEDENCE";
        const ID: &str = "cred-precedence-test-provider";
        let mut config = Config::default();
        config.providers.insert(
            ID.to_string(),
            ProviderConfig {
                kind: "openai-compatible".to_string(),
                api_key_env: Some(ENV.to_string()),
                ..ProviderConfig::default()
            },
        );

        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::with_file(Some(dir.path().join("credentials.json")));

        // With neither a stored credential nor the env var, resolution is None.
        std::env::remove_var(ENV);
        assert!(config.resolve_credential_with(ID, &store).is_none());

        // With only the env var, it resolves from the environment.
        std::env::set_var(ENV, "env-key");
        assert_eq!(
            config
                .resolve_credential_with(ID, &store)
                .map(|s| s.expose().to_string()),
            Some("env-key".to_string())
        );

        // A stored credential takes precedence over the env var.
        store.file_set(ID, &Secret::new("stored-key")).unwrap();
        assert_eq!(
            config
                .resolve_credential_with(ID, &store)
                .map(|s| s.expose().to_string()),
            Some("stored-key".to_string())
        );

        std::env::remove_var(ENV);
    }

    #[test]
    fn history_persistence_defaults_on_and_loads_none_from_project_toml() {
        use crate::schema::HistoryPersistence;

        // No [history] section anywhere ⇒ persistence is on by default.
        let on = load(&ConfigPaths::default(), &CliOverrides::default()).expect("load defaults");
        assert_eq!(on.history.persistence, HistoryPersistence::SaveAll);

        // A project file may opt out with the kebab-case `none`.
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join(".localpilot.toml");
        std::fs::write(&project, "[history]\npersistence = \"none\"\n").unwrap();
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let off = load(&paths, &CliOverrides::default()).expect("load config");
        assert_eq!(off.history.persistence, HistoryPersistence::None);
    }

    #[test]
    fn prompt_history_path_sits_beside_the_user_config() {
        // The store lives in the same per-user localpilot dir as config.toml.
        match (prompt_history_path(), user_config_path()) {
            (Some(history), Some(config)) => {
                assert_eq!(history.parent(), config.parent());
                assert_eq!(
                    history.file_name().and_then(|n| n.to_str()),
                    Some("prompt-history.jsonl")
                );
            }
            // No base dir on this host (no APPDATA/XDG/HOME): both resolve to None.
            (history, _) => assert!(history.is_none()),
        }
    }

    #[test]
    fn ingest_refresh_interval_defaults_and_loads_from_project_toml() {
        // Documented default: a 10-minute debounce between auto-refreshes.
        assert_eq!(
            crate::schema::IngestConfig::default().refresh_min_interval_secs,
            600
        );

        // A project may override it; the rest of the ingest config keeps defaults.
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join(".localpilot.toml");
        std::fs::write(&project, "[ingest]\nrefresh_min_interval_secs = 30\n").unwrap();
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };

        let config = load(&paths, &CliOverrides::default()).expect("load config");

        assert_eq!(config.ingest.refresh_min_interval_secs, 30);
        assert_eq!(
            config.ingest.max_files,
            crate::schema::IngestConfig::default().max_files
        );
    }
}
