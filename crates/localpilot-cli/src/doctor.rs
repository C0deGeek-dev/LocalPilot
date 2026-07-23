//! `localpilot doctor` — environment diagnostics.
//!
//! Data gathering ([`report`]) is deliberately separated from rendering
//! ([`render`]) so the human-readable output is deterministic and testable
//! without depending on the host environment. Credential *values* never enter
//! the report — only whether a credential is present — so no secret can reach
//! stdout or a snapshot.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use localpilot_config::{
    redact::redact, CliOverrides, ConfigPaths, CredentialSource, ProviderAuth,
};
use localpilot_mcp::{McpClient, StdioTransport, Transport};
use serde::Serialize;

const MCP_DOCTOR_TIMEOUT: Duration = Duration::from_secs(5);

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
    pub mcp_servers: Vec<McpServerStatus>,
    /// The resolved LocalMind store root (walked up from the cwd), when one exists.
    pub memory_root: Option<String>,
    /// Research-report → documentation-index state, when there is anything to
    /// report. Distinguishes "reports exist but report ingestion is disabled"
    /// from "ingestion enabled but nothing indexed" and "indexed without
    /// embeddings" — three states a bare empty doc search cannot explain.
    pub research_docs: Option<ResearchDocsStatus>,
    /// Stable capability tokens this build advertises, so a wrapper can
    /// feature-detect against an older binary rather than guess from the version.
    pub capabilities: Vec<String>,
    pub workspace_trust: TrustState,
}

/// The state of the research-report → doc-index bridge for the cwd project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResearchDocsStatus {
    /// Markdown research reports found under the research output directory.
    pub reports_found: usize,
    /// Whether `[research] ingest_report` is enabled.
    pub report_ingestion_enabled: bool,
    /// Documentation passages in the project's LocalMind doc index, when a
    /// usable store exists.
    pub doc_chunks: Option<i64>,
    /// How many of those passages carry an embedding vector.
    pub doc_vectors: Option<i64>,
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
    /// The provider's declared vision (image-input) capability, when set in
    /// config. `doctor` reads config offline, so this is the *declared* value;
    /// the discovery probe (and the full config-or-probe resolution) surfaces in
    /// `localpilot models`, which queries the server.
    pub supports_vision: Option<bool>,
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

/// One configured MCP server and the result of probing its stdio endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpServerStatus {
    pub name: String,
    pub command: String,
    pub arg_count: usize,
    pub command_available: bool,
    pub connected: bool,
    pub protocol_version: Option<String>,
    pub tool_count: usize,
    pub tools: Vec<String>,
    pub error: Option<String>,
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
        mcp_servers: Vec::new(),
        memory_root: memory_root(),
        research_docs: research_docs(),
        capabilities: capabilities(),
        workspace_trust: TrustState::Unknown,
    }
}

/// The research-report → doc-index state for the cwd project, or `None` when
/// there is nothing to report (no reports on disk, ingestion off, and no doc
/// index). Best-effort and read-only, like the rest of `doctor`.
fn research_docs() -> Option<ResearchDocsStatus> {
    let cwd = std::env::current_dir().ok()?;
    let reports_dir = cwd.join(".localpilot").join("research");
    let reports_found = std::fs::read_dir(&reports_dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "md"))
                .count()
        })
        .unwrap_or(0);
    let report_ingestion_enabled =
        localpilot_config::load(&ConfigPaths::standard(&cwd), &CliOverrides::default())
            .map(|config| config.research.ingest_report)
            .unwrap_or(false);
    let counts = localpilot_localmind::doc_index_counts(&cwd);
    if reports_found == 0 && !report_ingestion_enabled && counts.is_none() {
        return None;
    }
    Some(ResearchDocsStatus {
        reports_found,
        report_ingestion_enabled,
        doc_chunks: counts.map(|(chunks, _)| chunks),
        doc_vectors: counts.map(|(_, vectors)| vectors),
    })
}

/// Gather a diagnostics report including a bounded live MCP probe.
pub async fn report_with_mcp() -> DoctorReport {
    let mut report = report();
    report.mcp_servers = mcp_servers().await;
    report
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
pub async fn run(out: &mut dyn Write) -> io::Result<()> {
    run_with(out, crate::output::OutputFormat::Human).await
}

/// Gather a report and write it in the requested format. The JSON form is the
/// agent-consumable surface (ADR-0048's `--format` contract extended to `doctor`);
/// the human form is unchanged for an interactive caller.
///
/// # Errors
/// Returns any error from writing to `out`.
pub async fn run_with(out: &mut dyn Write, format: crate::output::OutputFormat) -> io::Result<()> {
    let report = report_with_mcp().await;
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
        // Vision is only shown when declared in config, so an undeclared provider
        // reads exactly as before.
        let vision = match p.supports_vision {
            Some(true) => "; vision declared",
            Some(false) => "; vision off (declared)",
            None => "",
        };
        let _ = writeln!(
            s,
            "  {} ({}): credential {} [{source}]{base}; model {model}; context window {window}{vision}",
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

    let _ = writeln!(s, "mcp servers:");
    if report.mcp_servers.is_empty() {
        let _ = writeln!(s, "  (none configured)");
    }
    for server in &report.mcp_servers {
        let args = format!("args: {}", server.arg_count);
        let command_state = if server.command_available {
            "command available"
        } else {
            "command not found"
        };
        if server.connected {
            let protocol = server
                .protocol_version
                .as_deref()
                .map(|version| format!("; protocol {version}"))
                .unwrap_or_default();
            let tools = summarize_mcp_tools(&server.tools);
            let _ = writeln!(
                s,
                "  {} ({}): connected{protocol}; {} tool(s): {tools} ({args}; {command_state})",
                server.name, server.command, server.tool_count
            );
        } else {
            let error = server.error.as_deref().unwrap_or("unknown error");
            let _ = writeln!(
                s,
                "  {} ({}): failed; {error} ({args}; {command_state})",
                server.name, server.command
            );
        }
    }
    let _ = writeln!(s);

    let memory = report.memory_root.as_deref().unwrap_or("(none resolved)");
    let _ = writeln!(s, "memory store: {memory}");
    if let Some(research) = &report.research_docs {
        let ingestion = if research.report_ingestion_enabled {
            "enabled"
        } else {
            "disabled ([research] ingest_report = false)"
        };
        let index = match (research.doc_chunks, research.doc_vectors) {
            (Some(chunks), Some(vectors)) => {
                format!("{chunks} chunk(s), {vectors} with embeddings")
            }
            _ => "(no usable doc index)".to_string(),
        };
        let _ = writeln!(
            s,
            "research docs: {} report(s) on disk; report ingestion {ingestion}; doc index: {index}",
            research.reports_found
        );
    }
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
        supports_vision: None,
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
                    supports_vision: entry.supports_vision,
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

async fn mcp_servers() -> Vec<McpServerStatus> {
    let Some(config) = resolved_config() else {
        return Vec::new();
    };
    let mut statuses = Vec::new();
    for (name, server) in &config.mcp.servers {
        statuses.push(probe_mcp_server(name, &server.command, &server.args).await);
    }
    statuses
}

fn resolved_config() -> Option<localpilot_config::Config> {
    let cwd = std::env::current_dir().ok()?;
    localpilot_config::load(&ConfigPaths::standard(&cwd), &CliOverrides::default()).ok()
}

async fn probe_mcp_server(name: &str, command: &str, args: &[String]) -> McpServerStatus {
    let command_available = command_available(command);
    let mut status = McpServerStatus {
        name: name.to_string(),
        command: command.to_string(),
        arg_count: args.len(),
        command_available,
        connected: false,
        protocol_version: None,
        tool_count: 0,
        tools: Vec::new(),
        error: None,
    };

    if !command_available {
        status.error = Some("command not found".to_string());
        return status;
    }

    let probe = async {
        let transport: Arc<dyn Transport> = Arc::new(StdioTransport::spawn(command, args)?);
        let client = McpClient::new(Arc::clone(&transport));
        let server_status = client.initialize().await?;
        let tools = client.list_tools().await?;
        Ok::<_, localpilot_mcp::McpError>((server_status, tools))
    };

    match tokio::time::timeout(MCP_DOCTOR_TIMEOUT, probe).await {
        Ok(Ok((server_status, tools))) => {
            status.connected = true;
            status.protocol_version = Some(server_status.protocol_version);
            status.tool_count = tools.len();
            status.tools = tools.into_iter().map(|tool| tool.name).collect();
        }
        Ok(Err(error)) => {
            status.error = Some(redact(&error.to_string()));
        }
        Err(_) => {
            status.error = Some(format!("timed out after {}s", MCP_DOCTOR_TIMEOUT.as_secs()));
        }
    }
    status
}

fn command_available(command: &str) -> bool {
    let path = Path::new(command);
    if path.is_absolute()
        || command.contains(std::path::MAIN_SEPARATOR)
        || command.contains('/')
        || command.contains('\\')
    {
        return path.is_file();
    }
    tool_on_path(command)
}

fn summarize_mcp_tools(tools: &[String]) -> String {
    if tools.is_empty() {
        return "(none)".to_string();
    }
    const MAX: usize = 6;
    let shown = tools
        .iter()
        .take(MAX)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if tools.len() > MAX {
        format!("{shown}, ... (+{} more)", tools.len() - MAX)
    } else {
        shown
    }
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
                    supports_vision: Some(true),
                },
                ProviderStatus {
                    name: "openai".to_string(),
                    kind: "openai".to_string(),
                    base_url: Some("https://api.openai.com/v1".to_string()),
                    credential_env: "OPENAI_API_KEY".to_string(),
                    credential_source: CredentialSource::Keychain,
                    model: None,
                    context_window: None,
                    supports_vision: None,
                },
            ],
            memory_root: Some("/work/.localmind".to_string()),
            research_docs: Some(ResearchDocsStatus {
                reports_found: 2,
                report_ingestion_enabled: false,
                doc_chunks: Some(0),
                doc_vectors: Some(0),
            }),
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
            mcp_servers: vec![McpServerStatus {
                name: "context7".to_string(),
                command: "npx".to_string(),
                arg_count: 2,
                command_available: true,
                connected: true,
                protocol_version: Some("2025-06-18".to_string()),
                tool_count: 1,
                tools: vec!["get-library-docs".to_string()],
                error: None,
            }],
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
        // The declared vision capability rides in the JSON for an agent to read;
        // an undeclared provider carries a null, never a guessed value.
        assert_eq!(parsed["providers"][0]["supports_vision"], true);
        assert!(parsed["providers"][1]["supports_vision"].is_null());
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
