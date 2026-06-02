//! `unshackled doctor` — environment diagnostics.
//!
//! Data gathering ([`report`]) is deliberately separated from rendering
//! ([`render`]) so the human-readable output is deterministic and testable
//! without depending on the host environment. Credential *values* never enter
//! the report — only whether a credential is present — so no secret can reach
//! stdout or a snapshot.

use std::io::{self, Write};
use std::path::PathBuf;

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
}

/// Whether an external tool the agent can use was found on `PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolStatus {
    pub name: String,
    pub command: String,
    pub available: bool,
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
        version: env!("CARGO_PKG_VERSION").to_string(),
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
    let _ = writeln!(s, "Unshackled {}", report.version);
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
        let _ = writeln!(s, "  {}: credential {} {state}", p.name, p.credential_env);
    }
    let _ = writeln!(s);

    let _ = writeln!(s, "tools:");
    for t in &report.tools {
        let state = if t.available {
            "available"
        } else {
            "not found"
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
        let project = cwd.join(".unshackled.toml");
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
        .map(|base| PathBuf::from(base).join("unshackled").join("config.toml"))
}

#[cfg(not(windows))]
fn user_config_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|base| base.join("unshackled").join("config.toml"))
}

/// Known providers and the environment variable that carries each credential.
fn providers() -> Vec<ProviderStatus> {
    [
        ("local", "UNSHACKLED_LOCAL_API_KEY"),
        ("openai", "OPENAI_API_KEY"),
    ]
    .into_iter()
    .map(|(name, env)| ProviderStatus {
        name: name.to_string(),
        credential_env: env.to_string(),
        credential_present: credential_present(env),
    })
    .collect()
}

fn credential_present(env: &str) -> bool {
    std::env::var(env)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// External tools the agent can use, checked by scanning `PATH`.
fn tools() -> Vec<ToolStatus> {
    [("git", "git"), ("ripgrep", "rg")]
        .into_iter()
        .map(|(name, command)| ToolStatus {
            name: name.to_string(),
            command: command.to_string(),
            available: tool_on_path(command),
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
                    path: "/config/unshackled/config.toml".to_string(),
                    exists: false,
                },
                ConfigPath {
                    label: "project".to_string(),
                    path: "/work/.unshackled.toml".to_string(),
                    exists: true,
                },
            ],
            providers: vec![
                ProviderStatus {
                    name: "local".to_string(),
                    credential_env: "UNSHACKLED_LOCAL_API_KEY".to_string(),
                    credential_present: false,
                },
                ProviderStatus {
                    name: "openai".to_string(),
                    credential_env: "OPENAI_API_KEY".to_string(),
                    credential_present: true,
                },
            ],
            tools: vec![
                ToolStatus {
                    name: "git".to_string(),
                    command: "git".to_string(),
                    available: true,
                },
                ToolStatus {
                    name: "ripgrep".to_string(),
                    command: "rg".to_string(),
                    available: false,
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
        std::env::set_var("OPENAI_API_KEY", secret);
        let rendered = render(&report());
        std::env::remove_var("OPENAI_API_KEY");

        assert!(
            !rendered.contains(secret),
            "credential value leaked into output"
        );
        assert!(rendered.contains("OPENAI_API_KEY"));
    }

    #[test]
    fn report_reads_real_environment_without_panicking() {
        let r = report();
        assert_eq!(r.version, env!("CARGO_PKG_VERSION"));
        assert!(r.providers.iter().any(|p| p.name == "openai"));
        assert!(r.tools.iter().any(|t| t.command == "git"));
    }
}
