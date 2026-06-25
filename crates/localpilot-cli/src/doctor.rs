//! `localpilot doctor` — environment diagnostics.
//!
//! Data gathering ([`report`]) is deliberately separated from rendering
//! ([`render`]) so the human-readable output is deterministic and testable
//! without depending on the host environment. Credential *values* never enter
//! the report — only whether a credential is present — so no secret can reach
//! stdout or a snapshot.

use std::io::{self, Write};
use std::path::PathBuf;

use localpilot_config::{CliOverrides, ConfigPaths, CredentialSource, ProviderAuth};
use serde::Serialize;

/// A point-in-time view of the local environment relevant to running the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorReport {
    /// The build's `git describe` version string (embedded at compile time).
    pub version: String,
    /// The resolved path of the running executable — the signal a wrapper compares
    /// against `version` to detect a stale PATH binary vs the repo build.
    pub binary_path: Option<String>,
    pub os: String,
    pub arch: String,
    pub config_paths: Vec<ConfigPath>,
    pub providers: Vec<ProviderStatus>,
    pub tools: Vec<ToolStatus>,
    /// The resolved LocalMind store root (walked up from the cwd), when one exists.
    pub memory_root: Option<String>,
    /// Stable capability tokens this build advertises, so a wrapper can
    /// feature-detect against an older binary rather than guess from the version.
    pub capabilities: Vec<String>,
    pub workspace_trust: TrustState,
}

/// A candidate configuration file location and whether it currently exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigPath {
    pub label: String,
    pub path: String,
    pub exists: bool,
}

/// Where a provider's credential resolves from, and the env var it would read.
/// The credential value itself is never stored here — only its source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderStatus {
    pub name: String,
    /// The provider kind (`anthropic`, `openai`, `openai-compatible`, …).
    pub kind: String,
    /// The configured base URL, when the provider sets one explicitly.
    pub base_url: Option<String>,
    pub credential_env: String,
    /// Which tier the credential resolves from (keychain / file / env / none).
    /// Serialized as a label string — never the secret value.
    #[serde(serialize_with = "serialize_credential_source")]
    pub credential_source: CredentialSource,
    /// The provider's default model, when configured.
    pub model: Option<String>,
    /// The model's context window in tokens, when configured.
    pub context_window: Option<u64>,
}

/// Map a credential source to its machine token (never the value).
fn credential_source_json(source: CredentialSource) -> &'static str {
    match source {
        CredentialSource::Keychain => "keychain",
        CredentialSource::File => "file",
        CredentialSource::Env => "env",
        CredentialSource::GoogleAdc => "google_adc",
        CredentialSource::GoogleAdcFile => "google_adc_file",
        CredentialSource::None => "none",
    }
}

fn serialize_credential_source<S: serde::Serializer>(
    source: &CredentialSource,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(credential_source_json(*source))
}

/// Whether an external tool the agent can use was found on `PATH`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolStatus {
    pub name: String,
    pub command: String,
    pub available: bool,
    /// Whether the agent works without this tool (it has a builtin equivalent).
    pub optional: bool,
}

/// Workspace trust state. Trust is established by the sandbox when a session
/// starts; `doctor` only reports what it can observe ahead of that.
// `Trusted`/`Untrusted` are produced by the sandbox trust check once a session
// evaluates the workspace; `doctor` reports `Unknown` until then.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustState {
    Trusted,
    Untrusted,
    Unknown,
}

/// Gather a diagnostics report from the current environment.
#[must_use]
pub fn report() -> DoctorReport {
    DoctorReport {
        version: env!("LOCALPILOT_VERSION").to_string(),
        binary_path: binary_path(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        config_paths: config_paths(),
        providers: providers(),
        tools: tools(),
        memory_root: memory_root(),
        capabilities: capabilities(),
        workspace_trust: TrustState::Unknown,
    }
}

/// The resolved path of the running executable, when discoverable. Paired with
/// `version` it lets a wrapper detect a stale PATH binary vs the repo build —
/// drift *detection* is the caller's job (this only reports the facts).
fn binary_path() -> Option<String> {
    std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
}

/// The resolved LocalMind store root, walked up from the cwd like the `learning`
/// and `memory` commands resolve it, or `None` when no store exists at or above.
fn memory_root() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let resolved = localpilot_localmind::resolve_store_root(&cwd);
    resolved
        .is_found()
        .then(|| resolved.path().display().to_string())
}

/// Stable capability tokens this build advertises. A wrapper checks for a token
/// to confirm a binary supports an agent-facing surface (e.g. the `--workspace`
/// flag a stale PATH binary lacked) instead of inferring it from the version.
/// Append-only: removing a token is a breaking change for a consumer.
fn capabilities() -> Vec<String> {
    let mut caps = vec![
        "doctor-json".to_string(),
        "models-json".to_string(),
        "learning-workspace-flag".to_string(),
        "print-turn-timeout".to_string(),
    ];
    if cfg!(feature = "tui") {
        caps.push("tui".to_string());
    }
    caps
}

/// Gather a report and write its human-readable form to `out`.
///
/// # Errors
/// Returns any error from writing to `out`.
pub fn run(out: &mut dyn Write) -> io::Result<()> {
    run_with(out, crate::output::OutputFormat::Human)
}

/// Gather a report and write it in the requested format. The JSON form is the
/// agent-consumable surface (ADR-0048's `--format` contract extended to `doctor`);
/// the human form is unchanged for an interactive caller.
///
/// # Errors
/// Returns any error from writing to `out`.
pub fn run_with(out: &mut dyn Write, format: crate::output::OutputFormat) -> io::Result<()> {
    let report = report();
    let rendered = match format {
        crate::output::OutputFormat::Human => render(&report),
        crate::output::OutputFormat::Json => render_json(&report),
    };
    out.write_all(rendered.as_bytes())
}

/// Render a report as a machine-readable JSON object (one trailing newline).
/// Serialization of the owned report is infallible; the fallback keeps the
/// function total without an `unwrap`/`expect` on the runtime path.
#[must_use]
pub fn render_json(report: &DoctorReport) -> String {
    let body = serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".to_string());
    format!("{body}\n")
}

/// Render a report as deterministic, human-readable text.
#[must_use]
pub fn render(report: &DoctorReport) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();

    // `writeln!` into a String is infallible; the result is intentionally ignored.
    let _ = writeln!(s, "LocalPilot {}", report.version);
    if let Some(path) = &report.binary_path {
        let _ = writeln!(s, "  binary: {path}");
    }
    let _ = writeln!(s);
    let _ = writeln!(s, "platform:");
    let _ = writeln!(s, "  os:   {}", report.os);
    let _ = writeln!(s, "  arch: {}", report.arch);
    let _ = writeln!(s);

    let _ = writeln!(s, "config search paths:");
    for c in &report.config_paths {
        let state = if c.exists { "present" } else { "missing" };
        let _ = writeln!(s, "  {}: {} ({state})", c.label, c.path);
    }
    let _ = writeln!(s);

    let _ = writeln!(s, "providers:");
    for p in &report.providers {
        // Report the credential *source*, never the secret: a logged-in key shows
        // `keychain`/`file`, an environment variable `env`, and nothing `none`.
        let source = match p.credential_source {
            CredentialSource::Keychain => "keychain",
            CredentialSource::File => "file",
            CredentialSource::Env => "env",
            CredentialSource::GoogleAdc => "google_adc",
            CredentialSource::GoogleAdcFile => "google_adc_file",
            CredentialSource::None => "not set",
        };
        let model = p.model.as_deref().unwrap_or("(none)");
        let window = p
            .context_window
            .map(|w| format!("{w} tokens"))
            .unwrap_or_else(|| "unknown".to_string());
        let base = p
            .base_url
            .as_deref()
            .map(|u| format!("; base {u}"))
            .unwrap_or_default();
        let _ = writeln!(
            s,
            "  {} ({}): credential {} [{source}]{base}; model {model}; context window {window}",
            p.name, p.kind, p.credential_env
        );
    }
    let _ = writeln!(s);

    let _ = writeln!(s, "tools:");
    for t in &report.tools {
        let state = match (t.available, t.optional) {
            (true, _) => "available",
            (false, true) => "not found (optional)",
            (false, false) => "not found",
        };
        let _ = writeln!(s, "  {} ({}): {state}", t.name, t.command);
    }
    let _ = writeln!(s);

    let memory = report.memory_root.as_deref().unwrap_or("(none resolved)");
    let _ = writeln!(s, "memory store: {memory}");
    let _ = writeln!(s, "capabilities: {}", report.capabilities.join(", "));
    let _ = writeln!(s);

    let trust = match report.workspace_trust {
        TrustState::Trusted => "trusted",
        TrustState::Untrusted => "untrusted",
        TrustState::Unknown => "unknown (evaluated when a session starts)",
    };
    let _ = writeln!(s, "workspace trust: {trust}");

    s
}

/// Candidate config file locations. Full precedence resolution lives in the
/// config layer; `doctor` only reports where files would be looked for.
fn config_paths() -> Vec<ConfigPath> {
    let mut paths = Vec::new();

    if let Some(user) = user_config_path() {
        paths.push(ConfigPath {
            label: "user".to_string(),
            exists: user.is_file(),
            path: user.display().to_string(),
        });
    }

    if let Ok(cwd) = std::env::current_dir() {
        let project = cwd.join(".localpilot.toml");
        paths.push(ConfigPath {
            label: "project".to_string(),
            exists: project.is_file(),
            path: project.display().to_string(),
        });
    }

    paths
}

#[cfg(windows)]
fn user_config_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA")
        .map(|base| PathBuf::from(base).join("localpilot").join("config.toml"))
}

#[cfg(not(windows))]
fn user_config_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|base| base.join("localpilot").join("config.toml"))
}

/// The configured providers (from `.localpilot.toml`) when any are set,
/// otherwise the conventional provider kinds and their credential env vars.
fn providers() -> Vec<ProviderStatus> {
    if let Some(configured) = configured_providers() {
        return configured;
    }
    [
        ("local", "LOCALPILOT_LOCAL_API_KEY"),
        ("openai", "OPENAI_API_KEY"),
        ("anthropic", "ANTHROPIC_API_KEY"),
    ]
    .into_iter()
    .map(|(name, env)| ProviderStatus {
        name: name.to_string(),
        kind: name.to_string(),
        base_url: None,
        credential_env: env.to_string(),
        // With no config there is no stored-credential lookup to do; presence is
        // read straight from the conventional environment variable.
        credential_source: if credential_present(env) {
            CredentialSource::Env
        } else {
            CredentialSource::None
        },
        model: None,
        context_window: None,
    })
    .collect()
}

/// Providers declared in the resolved configuration, or `None` when no config is
/// present or it declares no providers.
fn configured_providers() -> Option<Vec<ProviderStatus>> {
    let cwd = std::env::current_dir().ok()?;
    let config =
        localpilot_config::load(&ConfigPaths::standard(&cwd), &CliOverrides::default()).ok()?;
    if config.providers.is_empty() {
        return None;
    }
    Some(
        config
            .providers
            .iter()
            .map(|(id, entry)| {
                // The resolved source honours the full precedence (keychain →
                // fallback file → env), so a logged-in provider reads `keychain`
                // even with no environment variable set.
                let source = config.credential_source(id);
                let credential_env = if entry.auth == ProviderAuth::GoogleAdc {
                    if entry
                        .google_adc_path
                        .as_ref()
                        .is_some_and(|path| !path.trim().is_empty())
                    {
                        "google_adc_path".to_string()
                    } else {
                        "GOOGLE_APPLICATION_CREDENTIALS".to_string()
                    }
                } else {
                    entry
                        .api_key_env
                        .as_deref()
                        .or_else(|| default_api_key_env(&entry.kind))
                        .map(str::to_string)
                        .unwrap_or_else(|| "(none required)".to_string())
                };
                ProviderStatus {
                    name: id.clone(),
                    kind: entry.kind.clone(),
                    base_url: entry.base_url.clone(),
                    credential_env,
                    credential_source: source,
                    model: entry.model.clone(),
                    context_window: entry.context_window,
                }
            })
            .collect(),
    )
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

fn credential_present(env: &str) -> bool {
    std::env::var(env)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// External tools the agent can use, checked by scanning `PATH`. `ripgrep` is
/// optional — the builtin `search_text` tool searches in-process — and `sqlite3`
/// is optional too: the first-class LocalMind read tools cover inspecting the
/// store, so the agent never needs the CLI to read it.
fn tools() -> Vec<ToolStatus> {
    [
        ("git", "git", false),
        ("ripgrep", "rg", true),
        ("sqlite3", "sqlite3", true),
    ]
    .into_iter()
    .map(|(name, command, optional)| ToolStatus {
        name: name.to_string(),
        command: command.to_string(),
        available: tool_on_path(command),
        optional,
    })
    .collect()
}

/// Whether `command` resolves to an executable file on `PATH`.
fn tool_on_path(command: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let exts = executable_extensions();
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let mut candidate = dir.join(command);
            if !ext.is_empty() {
                candidate.set_extension(ext);
            }
            if candidate.is_file() {
                return true;
            }
        }
    }
    false
}

#[cfg(windows)]
fn executable_extensions() -> Vec<String> {
    std::env::var("PATHEXT")
        .map(|v| {
            v.split(';')
                .filter(|s| !s.is_empty())
                .map(|s| s.trim_start_matches('.').to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_else(|_| {
            ["exe", "cmd", "bat", "com"]
                .iter()
                .map(|s| (*s).to_string())
                .collect()
        })
}

#[cfg(not(windows))]
fn executable_extensions() -> Vec<String> {
    vec![String::new()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> DoctorReport {
        DoctorReport {
            version: "0.0.0-test".to_string(),
            binary_path: Some("/bin/localpilot".to_string()),
            os: "testos".to_string(),
            arch: "testarch".to_string(),
            config_paths: vec![
                ConfigPath {
                    label: "user".to_string(),
                    path: "/config/localpilot/config.toml".to_string(),
                    exists: false,
                },
                ConfigPath {
                    label: "project".to_string(),
                    path: "/work/.localpilot.toml".to_string(),
                    exists: true,
                },
            ],
            providers: vec![
                ProviderStatus {
                    name: "local".to_string(),
                    kind: "local".to_string(),
                    base_url: None,
                    credential_env: "LOCALPILOT_LOCAL_API_KEY".to_string(),
                    credential_source: CredentialSource::None,
                    model: None,
                    context_window: None,
                },
                ProviderStatus {
                    name: "openai".to_string(),
                    kind: "openai".to_string(),
                    base_url: Some("https://api.openai.com/v1".to_string()),
                    credential_env: "OPENAI_API_KEY".to_string(),
                    credential_source: CredentialSource::Keychain,
                    model: None,
                    context_window: None,
                },
            ],
            memory_root: Some("/work/.localmind".to_string()),
            capabilities: vec!["doctor-json".to_string(), "models-json".to_string()],
            tools: vec![
                ToolStatus {
                    name: "git".to_string(),
                    command: "git".to_string(),
                    available: true,
                    optional: false,
                },
                ToolStatus {
                    name: "ripgrep".to_string(),
                    command: "rg".to_string(),
                    available: false,
                    optional: true,
                },
                ToolStatus {
                    name: "sqlite3".to_string(),
                    command: "sqlite3".to_string(),
                    available: false,
                    optional: true,
                },
            ],
            workspace_trust: TrustState::Unknown,
        }
    }

    #[test]
    fn render_is_stable() {
        insta::assert_snapshot!(render(&fixture()));
    }

    #[test]
    fn render_json_is_stable() {
        insta::assert_snapshot!(render_json(&fixture()));
    }

    #[test]
    fn render_json_never_leaks_a_credential_value() {
        // The JSON carries the credential *source* token, never the secret.
        let json = render_json(&fixture());
        assert!(json.contains("\"credential_source\": \"keychain\""));
        assert!(json.contains("\"credential_source\": \"none\""));
        assert!(!json.contains("sk-"));
    }

    #[test]
    fn the_json_carries_drift_signals_and_capabilities() {
        // A wrapper detects PATH-vs-repo binary drift from the resolved exe path +
        // version, and feature-detects an agent surface from the capability tokens.
        let parsed: serde_json::Value =
            serde_json::from_str(&render_json(&fixture())).expect("doctor JSON parses");
        assert_eq!(parsed["version"], "0.0.0-test");
        assert_eq!(parsed["binary_path"], "/bin/localpilot");
        assert_eq!(parsed["memory_root"], "/work/.localmind");
        assert!(parsed["capabilities"]
            .as_array()
            .expect("capabilities is an array")
            .iter()
            .any(|c| c == "doctor-json"));
        assert_eq!(parsed["providers"][1]["kind"], "openai");
        assert_eq!(
            parsed["providers"][1]["base_url"],
            "https://api.openai.com/v1"
        );
    }

    #[test]
    fn render_never_leaks_credential_values() {
        // A present credential must be reported as presence only, never echoed.
        let secret = "sk-do-not-print-me";
        let rendered = render(&fixture());

        assert!(
            !rendered.contains(secret),
            "credential value leaked into output"
        );
        assert!(rendered.contains("OPENAI_API_KEY"));
    }

    #[test]
    fn render_reports_the_credential_source_per_provider() {
        // The fixture has a keychain-backed and a missing credential; the render
        // shows each source label and never a secret.
        let rendered = render(&fixture());
        assert!(rendered.contains("OPENAI_API_KEY [keychain]"));
        assert!(rendered.contains("LOCALPILOT_LOCAL_API_KEY [not set]"));
    }

    #[test]
    fn report_reads_real_environment_without_panicking() {
        let r = report();
        assert_eq!(r.version, env!("LOCALPILOT_VERSION"));
        assert!(!r.providers.is_empty());
        assert!(r.tools.iter().any(|t| t.command == "git"));
    }

    #[test]
    fn report_probes_sqlite3_as_an_optional_tool() {
        // The row is always present and flagged optional, regardless of whether
        // sqlite3 happens to be installed on the host running the test.
        let sqlite = report()
            .tools
            .into_iter()
            .find(|t| t.command == "sqlite3")
            .expect("sqlite3 must be probed");
        assert_eq!(sqlite.name, "sqlite3");
        assert!(
            sqlite.optional,
            "sqlite3 is optional — the builtin read tools cover the store"
        );
    }
}
