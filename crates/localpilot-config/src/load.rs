//! Configuration loading and precedence.
//!
//! Precedence, highest first: CLI flags, environment variables, the project
//! `.localpilot.toml`, the user config file, then built-in defaults.
//!
//! *Provider* credentials are never read from config files — only the *name* of
//! the environment variable holding each is configured, and the value is
//! resolved at use into a [`Secret`]. A configured MCP server is the one
//! documented exception: `[mcp.servers.<name>.env]` may carry a sensitive
//! literal in a project-local file, which is parsed straight into a masked
//! wrapper. The recommended form there names a credential-store entry instead,
//! so the file holds only an alias.

use std::path::{Path, PathBuf};

use figment::providers::{Env, Format, Serialized, Toml};
use figment::Figment;
use localpilot_core::Secret;

use crate::error::ConfigError;
use crate::schema::{CheckConfig, Config, McpEnvEntry, PermissionProfile, ProviderAuth};

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
    // The layers below the project, kept for the one key the project may only
    // ask for (see `separate_project_request`).
    let mut below_project = None;
    if let Some(project) = &paths.project {
        if project.is_file() {
            refuse_user_only_keys(project)?;
            below_project = Some(figment.clone());
            figment = figment.merge(Toml::file(project));
        }
    }

    figment = figment.merge(Env::prefixed("LOCALPILOT_").split("__"));

    if let Some(provider) = &cli.provider_default {
        figment = figment.merge(Serialized::default("provider.default", provider));
    }
    if let Some(profile) = &cli.permission_profile {
        figment = figment.merge(Serialized::default("permissions.profile", profile));
    }

    let mut config: Config = figment.extract().map_err(ConfigError::from)?;
    if let (Some(project), Some(below)) = (&paths.project, below_project) {
        separate_project_request(&mut config, project, below, cli)?;
    }
    synthesize_env_providers(&mut config);
    validate_checks(&config.harness.checks)?;
    validate_granularity(&config.harness.granularity)?;
    // Deliberately after the merge: layers combine per key, so an entry can be
    // invalid only once every layer has been applied (see `validate_mcp_env`).
    validate_mcp_env(&config.mcp)?;
    Ok(config)
}

/// A project file may ask for broad action authority but never grant it to
/// itself. Both ways of asking, the dedicated key and the legacy `bypass` or
/// `unrestricted` profile, become a [`ProjectRequest`] on the config, and the
/// project's own profile does not apply: the profile is what the layers below the
/// project, the environment and the command line choose. Narrowing (`readonly`,
/// `default`, `relaxed`) stays project-settable. The request takes effect only
/// through the user's authorization for that project.
fn separate_project_request(
    config: &mut Config,
    project: &Path,
    below_project: Figment,
    cli: &CliOverrides,
) -> Result<(), ConfigError> {
    use crate::schema::{ActionMode, ProjectRequest, ProjectRequestSource};

    let layer = Figment::from(Toml::file(project));
    let mut request = None;
    if layer
        .extract_inner::<ActionMode>("permissions.requested_action_mode")
        .ok()
        == Some(ActionMode::AcceptAll)
    {
        request = Some(ProjectRequest {
            mode: ActionMode::AcceptAll,
            source: ProjectRequestSource::RequestedActionMode,
        });
    }
    config.permissions.requested_action_mode = None;
    let source = match layer
        .extract_inner::<PermissionProfile>("permissions.profile")
        .ok()
    {
        Some(PermissionProfile::Unrestricted) => Some(ProjectRequestSource::UnrestrictedProfile),
        Some(PermissionProfile::Bypass) => Some(ProjectRequestSource::BypassProfile),
        _ => None,
    };
    if let Some(source) = source {
        let mut without_project = below_project.merge(Env::prefixed("LOCALPILOT_").split("__"));
        if let Some(profile) = &cli.permission_profile {
            without_project =
                without_project.merge(Serialized::default("permissions.profile", profile));
        }
        config.permissions.profile = without_project
            .extract_inner::<PermissionProfile>("permissions.profile")
            .map_err(ConfigError::from)?;
        request.get_or_insert(ProjectRequest {
            mode: ActionMode::AcceptAll,
            source,
        });
    }
    config.project_request = request;
    Ok(())
}

/// Keys a project file must never set: they grant the agent more than the
/// user's own settings do, and a repository must not be able to grant itself
/// anything. Refused loudly rather than ignored, so the author sees why.
const USER_ONLY_KEYS: [&str; 7] = [
    "permissions.action_mode",
    "permissions.allow_commands",
    "permissions.scratch_root",
    "harness.granularity.headless_units",
    "harness.granularity.headless_rename_units",
    "harness.commit_unverified",
    // The whole table: a repository must not choose where pairing points.
    "remote",
];

fn refuse_user_only_keys(project: &Path) -> Result<(), ConfigError> {
    let layer = Figment::from(Toml::file(project));
    for key in USER_ONLY_KEYS {
        if layer.find_value(key).is_ok() {
            return Err(ConfigError::UserOnly {
                path: project.display().to_string(),
                key,
            });
        }
    }
    Ok(())
}

/// Validate every `[mcp.servers.<name>.env]` entry.
///
/// This runs on the *merged* configuration, never per file, and that ordering is
/// load-bearing rather than incidental. Config layers combine per key, so a user
/// file supplying `KEY = { credential = "a" }` and a project file supplying
/// `KEY = { value = "b" }` — each perfectly valid alone — merge into one entry
/// carrying both. Only a post-merge check ever sees that.
///
/// Messages name the server and the variable, never a configured value.
fn validate_mcp_env(mcp: &crate::schema::McpConfig) -> Result<(), ConfigError> {
    for (server, config) in &mcp.servers {
        let mut folded: std::collections::HashMap<String, &str> = std::collections::HashMap::new();
        for (name, entry) in &config.env {
            if !is_portable_env_name(name) {
                return Err(ConfigError::InvalidMcpEnv(format!(
                    "server {server:?}: {name:?} is not a portable environment variable name \
                     (expected a letter or underscore followed by letters, digits, or underscores)"
                )));
            }
            // Windows matches environment names case-insensitively while unix does
            // not, so two entries differing only by case mean different things per
            // platform. Reject the ambiguity rather than resolve it differently on
            // each host (ADR-0007 is behaviour parity, not source parity).
            let key = name.to_ascii_uppercase();
            if let Some(previous) = folded.insert(key, name.as_str()) {
                return Err(ConfigError::InvalidMcpEnv(format!(
                    "server {server:?}: {previous:?} and {name:?} differ only by case, which is \
                     one variable on Windows and two on Linux/macOS — use a single spelling"
                )));
            }

            let McpEnvEntry::Object(object) = entry else {
                continue;
            };
            match (&object.value, &object.credential) {
                (Some(_), Some(_)) => {
                    return Err(ConfigError::InvalidMcpEnv(format!(
                        "server {server:?}: {name:?} sets both \"value\" and \"credential\" — \
                         set exactly one (note that separate config layers merge per key, so \
                         each may come from a different file)"
                    )))
                }
                (None, None) => {
                    return Err(ConfigError::InvalidMcpEnv(format!(
                        "server {server:?}: {name:?} sets neither \"value\" nor \"credential\""
                    )))
                }
                (None, Some(alias)) if !is_portable_credential_alias(alias) => {
                    return Err(ConfigError::InvalidMcpEnv(format!(
                        "server {server:?}: {name:?} names an invalid credential alias \
                         (expected letters, digits, \".\", \"-\", or \"_\")"
                    )))
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Whether `name` is a portable environment-variable name: a letter or
/// underscore followed by letters, digits, or underscores. Deliberately narrower
/// than any single platform allows, so a name that works on one tier-1 host
/// works on all three.
fn is_portable_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether `alias` is a portable credential-store alias. The conservative set
/// keeps an alias usable as an OS keychain entry name on every tier-1 platform.
///
/// Public so the `credential` CLI validates against this exact rule rather than
/// a second copy that could drift from what config accepts.
#[must_use]
pub fn is_portable_credential_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Validate the ratified quality-gate checks: each needs a non-empty name and
/// program, and names must be unique (a name is also a per-check override key).
fn validate_granularity(caps: &crate::schema::GranularityConfig) -> Result<(), ConfigError> {
    if caps.headless_rename_units > crate::schema::MAX_HEADLESS_UNITS {
        return Err(ConfigError::InvalidGranularity(format!(
            "headless_rename_units is {}; the maximum is {}",
            caps.headless_rename_units,
            crate::schema::MAX_HEADLESS_UNITS
        )));
    }

    if caps.headless_units > crate::schema::MAX_HEADLESS_UNITS {
        return Err(ConfigError::InvalidGranularity(format!(
            "headless_units is {}; the maximum is {}",
            caps.headless_units,
            crate::schema::MAX_HEADLESS_UNITS
        )));
    }
    Ok(())
}

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
    fn action_mode_is_user_owned() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user.toml");
        let project = dir.path().join(".localpilot.toml");
        let paths = ConfigPaths {
            user: Some(user.clone()),
            project: Some(project.clone()),
        };
        let load_mode =
            || load(&paths, &CliOverrides::default()).map(|config| config.permissions.action_mode);
        assert_eq!(load_mode().unwrap(), None);
        for (value, mode) in [
            ("normal", crate::ActionMode::Normal),
            ("accept_all", crate::ActionMode::AcceptAll),
        ] {
            let toml = format!("[permissions]\naction_mode = {value:?}\n");
            std::fs::write(&user, &toml).unwrap();
            assert_eq!(load_mode().unwrap(), Some(mode));
            std::fs::write(&project, &toml).unwrap();
            assert!(matches!(
                load_mode(),
                Err(ConfigError::UserOnly {
                    key: "permissions.action_mode",
                    ..
                })
            ));
            std::fs::remove_file(&project).unwrap();
        }
    }

    #[test]
    fn a_project_can_ask_for_broad_authority_but_not_grant_it_to_itself() {
        use crate::{ActionMode, ProjectRequestSource};

        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user.toml");
        let project = dir.path().join(".localpilot.toml");
        let paths = ConfigPaths {
            user: Some(user.clone()),
            project: Some(project.clone()),
        };
        let load_with = |user_toml: &str, project_toml: &str, cli: &CliOverrides| {
            std::fs::write(&user, user_toml).unwrap();
            std::fs::write(&project, project_toml).unwrap();
            load(&paths, cli).unwrap()
        };
        let none = CliOverrides::default();

        // The legacy profile keys are requests: the project's profile does not apply.
        for (value, source) in [
            ("unrestricted", ProjectRequestSource::UnrestrictedProfile),
            ("bypass", ProjectRequestSource::BypassProfile),
        ] {
            let toml = format!(
                "[permissions]
profile = {value:?}
"
            );
            let config = load_with("", &toml, &none);
            assert_eq!(config.permissions.profile, PermissionProfile::Default);
            let request = config.project_request.unwrap();
            assert_eq!(
                (request.mode, request.source),
                (ActionMode::AcceptAll, source)
            );

            // What the user's own layers choose stays, so a project cannot widen it.
            let config = load_with(
                "[permissions]
profile = \"relaxed\"
",
                &toml,
                &none,
            );
            assert_eq!(config.permissions.profile, PermissionProfile::Relaxed);
            assert!(config.project_request.is_some());
        }

        // The dedicated key is a request too, and is never an effective setting.
        let config = load_with(
            "",
            "[permissions]
requested_action_mode = \"accept_all\"
",
            &none,
        );
        assert_eq!(config.permissions.profile, PermissionProfile::Default);
        assert_eq!(config.permissions.action_mode, None);
        assert_eq!(config.permissions.requested_action_mode, None);
        assert_eq!(
            config.project_request.unwrap().source,
            ProjectRequestSource::RequestedActionMode
        );

        // Narrowing is the project's to choose and asks for nothing.
        for value in ["readonly", "default", "relaxed"] {
            let config = load_with(
                "",
                &format!(
                    "[permissions]
profile = {value:?}
"
                ),
                &none,
            );
            assert!(config.project_request.is_none(), "{value}");
        }
        let config = load_with(
            "",
            "[permissions]
requested_action_mode = \"normal\"
",
            &none,
        );
        assert!(config.project_request.is_none());
        let config = load_with(
            "",
            "[permissions]
profile = \"readonly\"
",
            &none,
        );
        assert_eq!(config.permissions.profile, PermissionProfile::Readonly);

        // The user's own choices, in the file or on the command line, are theirs.
        let config = load_with(
            "[permissions]
profile = \"unrestricted\"
",
            "[permissions]
profile = \"unrestricted\"
",
            &none,
        );
        assert_eq!(config.permissions.profile, PermissionProfile::Unrestricted);
        let cli = CliOverrides {
            permission_profile: Some(PermissionProfile::Unrestricted),
            ..CliOverrides::default()
        };
        let config = load_with(
            "",
            "[permissions]
profile = \"bypass\"
",
            &cli,
        );
        assert_eq!(config.permissions.profile, PermissionProfile::Unrestricted);

        // No request without a project file.
        std::fs::remove_file(&project).unwrap();
        assert!(load(&paths, &none).unwrap().project_request.is_none());
    }

    #[test]
    fn work_limit_mode_defaults_and_rejects_unknown_values() {
        use crate::{GranularityConfig, WorkLimitMode};

        assert_eq!(
            toml::from_str::<GranularityConfig>("").unwrap().mode,
            WorkLimitMode::Automatic
        );
        for (value, mode) in [
            ("automatic", WorkLimitMode::Automatic),
            ("strict", WorkLimitMode::Strict),
            ("wide", WorkLimitMode::Wide),
            ("very_wide", WorkLimitMode::VeryWide),
            ("off", WorkLimitMode::Off),
        ] {
            let parsed = toml::from_str::<GranularityConfig>(&format!("mode = '{value}'")).unwrap();
            assert_eq!(parsed.mode, mode);
        }
        assert!(toml::from_str::<GranularityConfig>("mode = 'custom'").is_err());
    }

    #[test]
    fn selective_rename_units_default_on_and_an_explicit_zero_survives() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user.toml");
        let paths = ConfigPaths {
            user: Some(user.clone()),
            project: None,
        };
        // No user file and no section: the eval-only selective mode is on;
        // global units stay off.
        let config = load(&paths, &CliOverrides::default()).unwrap();
        assert_eq!(config.harness.granularity.headless_rename_units, 8);
        assert_eq!(config.harness.granularity.headless_units, 0);

        // The user opts out with an explicit 0.
        std::fs::write(
            &user,
            "[harness.granularity]
headless_rename_units = 0
",
        )
        .unwrap();
        let config = load(&paths, &CliOverrides::default()).unwrap();
        assert_eq!(config.harness.granularity.headless_rename_units, 0);

        // Serializing that configuration (as the loader does for its own
        // defaults layer) keeps the explicit 0 instead of reverting to 8.
        let reloaded: Config = Figment::from(Serialized::defaults(config))
            .extract()
            .unwrap();
        assert_eq!(reloaded.harness.granularity.headless_rename_units, 0);
        let default_reloaded: Config = Figment::from(Serialized::defaults(Config::default()))
            .extract()
            .unwrap();
        assert_eq!(
            default_reloaded.harness.granularity.headless_rename_units,
            8
        );
    }

    #[test]
    fn saving_an_unrelated_change_keeps_the_rename_units_opt_out() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user.toml");
        std::fs::write(
            &user,
            "[harness.granularity]
headless_rename_units = 0
",
        )
        .unwrap();
        let paths = ConfigPaths {
            user: Some(user.clone()),
            project: None,
        };
        let mut config = load(&paths, &CliOverrides::default()).unwrap();
        config.provider.default = "elsewhere".to_string();
        std::fs::write(&user, toml::to_string(&config).unwrap()).unwrap();
        let saved = load(&paths, &CliOverrides::default()).unwrap();
        assert_eq!(saved.provider.default, "elsewhere");
        assert_eq!(saved.harness.granularity.headless_rename_units, 0);
    }

    #[test]
    fn allow_commands_come_from_the_user_config_and_never_from_the_project() {
        // Bug it prevents: a repository granting its own agent a vetted
        // command by shipping one in `.localpilot.toml`.
        let dir = tempfile::tempdir().unwrap();
        let entry = "[[permissions.allow_commands]]
program = \"python\"
args_prefix = [\"/abs/pair.py\"]
";
        let user = dir.path().join("user.toml");
        std::fs::write(&user, entry).unwrap();
        let project = dir.path().join(".localpilot.toml");

        std::fs::write(
            &project,
            "[permissions]
profile = \"readonly\"
",
        )
        .unwrap();
        let paths = ConfigPaths {
            user: Some(user.clone()),
            project: Some(project.clone()),
        };
        let config = load(&paths, &CliOverrides::default()).unwrap();
        assert_eq!(config.permissions.profile, PermissionProfile::Readonly);
        assert_eq!(config.permissions.allow_commands.len(), 1);
        assert_eq!(config.permissions.allow_commands[0].program, "python");

        std::fs::write(&project, entry).unwrap();
        let err = load(&paths, &CliOverrides::default()).expect_err("project grant refused");
        assert!(
            matches!(
                err,
                ConfigError::UserOnly {
                    key: "permissions.allow_commands",
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn a_project_may_choose_verification_but_only_the_user_authorizes_unverified_commits() {
        use crate::VerificationPolicy;
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user.toml");
        let project = dir.path().join("project.toml");
        std::fs::write(
            &user,
            "[harness]
commit_unverified = true
",
        )
        .unwrap();
        std::fs::write(
            &project,
            "[harness]
verification = \"manual\"
",
        )
        .unwrap();
        let paths = ConfigPaths {
            user: Some(user),
            project: Some(project.clone()),
        };
        let config = load(&paths, &CliOverrides::default()).unwrap();
        assert_eq!(config.harness.verification, VerificationPolicy::Manual);
        assert!(config.harness.commit_unverified);

        std::fs::write(
            &project,
            "[harness]
commit_unverified = true
",
        )
        .unwrap();
        let err = load(&paths, &CliOverrides::default()).expect_err("project grant refused");
        assert!(
            matches!(
                err,
                ConfigError::UserOnly {
                    key: "harness.commit_unverified",
                    ..
                }
            ),
            "{err}"
        );
        let defaults = Config::default().harness;
        assert_eq!(defaults.verification, VerificationPolicy::Current);
        assert!(!defaults.commit_unverified);
    }

    #[test]
    fn the_remote_gateway_is_set_only_by_the_user() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user.toml");
        let project = dir.path().join("project.toml");
        let paths = ConfigPaths {
            user: Some(user.clone()),
            project: Some(project.clone()),
        };
        std::fs::write(&project, "").unwrap();
        let config = load(&paths, &CliOverrides::default()).unwrap();
        assert!(!config.remote.enabled, "off unless the user turns it on");
        assert_eq!(config.remote.port, 47610);

        std::fs::write(
            &user,
            "[remote]
enabled = true
advertised_url = 'https://pc.example.ts.net'
",
        )
        .unwrap();
        let config = load(&paths, &CliOverrides::default()).unwrap();
        assert!(config.remote.enabled);
        assert_eq!(
            config.remote.advertised_url.as_deref(),
            Some("https://pc.example.ts.net")
        );

        // Any key of the table, from a project, is refused.
        for entry in [
            "[remote]
advertised_url = 'https://attacker.example'
",
            "[remote]
enabled = false
",
        ] {
            std::fs::write(&project, entry).unwrap();
            assert!(matches!(
                load(&paths, &CliOverrides::default()),
                Err(ConfigError::UserOnly { key: "remote", .. })
            ));
        }
    }

    #[test]
    fn scratch_policy_is_user_owned_and_cannot_be_granted_by_a_project() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user.toml");
        let project = dir.path().join("project.toml");
        let paths = ConfigPaths {
            user: Some(user.clone()),
            project: Some(project.clone()),
        };
        std::fs::write(&project, "").unwrap();
        for (value, expected) in [
            ("true", crate::ScratchRootConfig::Enabled(true)),
            ("false", crate::ScratchRootConfig::Enabled(false)),
            (
                "'/custom/scratch'",
                crate::ScratchRootConfig::Parent("/custom/scratch".into()),
            ),
        ] {
            std::fs::write(&user, format!("[permissions]\nscratch_root = {value}\n")).unwrap();
            assert_eq!(
                load(&paths, &CliOverrides::default())
                    .unwrap()
                    .permissions
                    .scratch_root,
                expected
            );
        }
        std::fs::write(&project, "[permissions]\nscratch_root = true\n").unwrap();
        assert!(matches!(
            load(&paths, &CliOverrides::default()),
            Err(ConfigError::UserOnly {
                key: "permissions.scratch_root",
                ..
            })
        ));
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
    fn the_broker_is_on_unless_a_layer_sets_it_off() {
        // No [tools] anywhere: on.
        let on = load(&ConfigPaths::default(), &CliOverrides::default()).expect("load defaults");
        assert!(on.tools.broker);

        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user.toml");
        let project = dir.path().join(".localpilot.toml");
        // A [tools] block that does not mention the broker keeps it on.
        std::fs::write(
            &project,
            "[tools]
learning = false
",
        )
        .unwrap();
        let partial = load(
            &ConfigPaths {
                user: None,
                project: Some(project.clone()),
            },
            &CliOverrides::default(),
        )
        .expect("load config");
        assert!(partial.tools.broker);
        // An explicit `false` in the user file turns it off ...
        std::fs::write(
            &user,
            "[tools]
broker = false
",
        )
        .unwrap();
        let off = load(
            &ConfigPaths {
                user: Some(user.clone()),
                project: None,
            },
            &CliOverrides::default(),
        )
        .expect("load config");
        assert!(!off.tools.broker);
        // ... and a project layer setting it explicitly wins over the user layer.
        std::fs::write(
            &project,
            "[tools]
broker = true
",
        )
        .unwrap();
        let back_on = load(
            &ConfigPaths {
                user: Some(user),
                project: Some(project),
            },
            &CliOverrides::default(),
        )
        .expect("load config");
        assert!(back_on.tools.broker);
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
