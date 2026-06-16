//! `localpilot doctor` — environment diagnostics.
//!
//! Data gathering ([`report`]) is deliberately separated from rendering
//! ([`render`]) so the human-readable output is deterministic and testable
//! without depending on the host environment. Credential *values* never enter
//! the report — only whether a credential is present — so no secret can reach
//! stdout or a snapshot.

use std::io::{self, Write};
use std::path::PathBuf;

use localpilot_config::{CliOverrides, ConfigPaths};

/// A point-in-time view of the local environment relevant to running the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    pub version: String,
    pub os: String,
    pub arch: String,
    pub config_paths: Vec<ConfigPath>,
    pub providers: Vec<ProviderStatus>,
    pub tools: Vec<ToolStatus>,
    pub workspace_trust: TrustState,
}

/// A candidate configuration file location and whether it currently exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPath {
    pub label: String,
    pub path: String,
    pub exists: bool,
}

/// Whether a provider's credential is present in the environment. The credential
/// value is never stored here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderStatus {
    pub name: String,
    pub credential_env: String,
    pub credential_present: bool,
    /// The provider's default model, when configured.
    pub model: Option<String>,
    /// The model's context window in tokens, when configured.
    pub context_window: Option<u64>,
}

/// Whether an external tool the agent can use was found on `PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        config_paths: config_paths(),
        providers: providers(),
        tools: tools(),
        workspace_trust: TrustState::Unknown,
    }
}

/// Gather a report and write its rendered form to `out`.
///
/// # Errors
/// Returns any error from writing to `out`.
pub fn run(out: &mut dyn Write) -> io::Result<()> {
    out.write_all(render(&report()).as_bytes())
}

/// Render a report as deterministic, human-readable text.
#[must_use]
pub fn render(report: &DoctorReport) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();

    // `writeln!` into a String is infallible; the result is intentionally ignored.
    let _ = writeln!(s, "LocalPilot {}", report.version);
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
        let state = if p.credential_present {
            "set"
        } else {
            "not set"
        };
        let model = p.model.as_deref().unwrap_or("(none)");
        let window = p
            .context_window
            .map(|w| format!("{w} tokens"))
            .unwrap_or_else(|| "unknown".to_string());
        let _ = writeln!(
            s,
            "  {}: credential {} {state}; model {model}; context window {window}",
            p.name, p.credential_env
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
        credential_env: env.to_string(),
        credential_present: credential_present(env),
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
                if let Some(env) = entry
                    .api_key_env
                    .as_deref()
                    .or_else(|| default_api_key_env(&entry.kind))
                {
                    return ProviderStatus {
                        name: format!("{id} ({})", entry.kind),
                        credential_env: env.to_string(),
                        credential_present: credential_present(env),
                        model: entry.model.clone(),
                        context_window: entry.context_window,
                    };
                }
                ProviderStatus {
                    name: format!("{id} ({})", entry.kind),
                    credential_env: "(none required)".to_string(),
                    credential_present: true,
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
                    credential_env: "LOCALPILOT_LOCAL_API_KEY".to_string(),
                    credential_present: false,
                    model: None,
                    context_window: None,
                },
                ProviderStatus {
                    name: "openai".to_string(),
                    credential_env: "OPENAI_API_KEY".to_string(),
                    credential_present: true,
                    model: None,
                    context_window: None,
                },
            ],
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
