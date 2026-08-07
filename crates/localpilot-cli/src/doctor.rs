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
use localpilot_mcp::McpClient;
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
    /// Subagent definitions visible from the cwd: how many resolved, and every
    /// file that failed to load. A definition that silently does not load is the
    /// failure this line exists to make visible.
    pub agents: AgentsStatus,
    /// Stable capability tokens this build advertises, so a wrapper can
    /// feature-detect against an older binary rather than guess from the version.
    pub capabilities: Vec<String>,
    pub workspace_trust: TrustState,
    /// The trusted-folders store path trust was evaluated against, when a config
    /// base resolves. `None` when no config base is available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_trust_store: Option<String>,
    /// Installed-skill-package discovery state: the `autonomous_discovery` flag
    /// and the readable catalog counts (or unreadable). Whether the project
    /// overlay is included, and the overlay-hidden wording, derive from
    /// [`DoctorReport::workspace_trust`] (the single trust authority) — so
    /// `skills.readable` is the trust-gated snapshot (effective global+project
    /// when trusted, global baseline only otherwise).
    pub skills: SkillsDoctor,
    /// Context-hygiene report — the authored-context layers (instruction files +
    /// skills) with their token weights and any advisory findings. Populated only
    /// by `doctor --hygiene`; absent (and omitted from JSON) otherwise, so the
    /// default report is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hygiene: Option<localpilot_contextcheck::ContextReport>,
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

/// What `doctor` could determine about one configured MCP server.
///
/// One classification drives both renderings, so the human and JSON views cannot
/// disagree about a server's state — and, more importantly, cannot disagree
/// about what is safe to print. Ordered by how early the check fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerState {
    /// The configured command is not on `PATH`. Nothing else was attempted.
    CommandUnavailable,
    /// A configured environment entry references a credential that is not
    /// stored. The server was deliberately not started.
    CredentialMissing,
    /// The process started but the handshake or tool listing failed.
    StartupFailed,
    /// Handshake and tool discovery both succeeded.
    Connected,
}

impl McpServerState {
    /// A short, secret-free label for the human rendering.
    fn label(self) -> &'static str {
        match self {
            McpServerState::CommandUnavailable => "command not found",
            McpServerState::CredentialMissing => "credential missing",
            McpServerState::StartupFailed => "failed to start",
            McpServerState::Connected => "connected",
        }
    }
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
    /// Names of the environment variables this server is configured to receive.
    /// Names only — a configured value never appears in `doctor` output, in
    /// either rendering.
    pub env_names: Vec<String>,
    /// What the probe concluded. `command_available` and `connected` are views
    /// of this, kept for the established JSON shape.
    pub state: McpServerState,
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
/// Subagent-definition health for the cwd project.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AgentsStatus {
    /// Effective definitions (after precedence).
    pub resolved: usize,
    /// Definitions shadowed by a higher-precedence file of the same name.
    pub shadowed: usize,
    /// `path: reason` for every file that could not be loaded.
    pub errors: Vec<String>,
}

/// Readable counts of the installed skill-package catalog. `0` is a real,
/// confident empty catalog — distinct from an unreadable one (see
/// [`SkillsDoctor::readable`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SkillsCounts {
    /// Discoverable (model-reachable) packages in the readable effective set.
    pub discoverable: usize,
    /// User-only (`disable-model-invocation: true`) packages — hidden from model
    /// discovery, reported only as a count.
    pub user_only: usize,
    /// Package entries the scan skipped as malformed/unreadable (a count only —
    /// never a name, description, or path). A nonzero value means a confident
    /// `discoverable`/`user_only` of `0` is NOT necessarily an empty catalog.
    pub skipped: usize,
}

/// Installed-skill-package state for `doctor`. All counting happens at report
/// construction; rendering reads these stored values only. Whether the project
/// overlay was included is derived at render from [`DoctorReport::workspace_trust`]
/// (the one trust authority) — not stored here, so it can never disagree with the
/// report's own trust block.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SkillsDoctor {
    /// The `[skills] autonomous_discovery` flag — whether the model-callable
    /// discovery tools are registered. A config-only value.
    pub autonomous_discovery: bool,
    /// The readable catalog counts, or `None` when the catalog could not be
    /// scanned at all (unreadable/unknown — never a confident zero). Under an
    /// untrusted or unknown-trust workspace this reflects the user-global baseline
    /// only (the project overlay is not read).
    pub readable: Option<SkillsCounts>,
}

/// Count the installed skill-package catalog for `doctor`, trust-safely, against
/// an explicit global `home` (the injectable seam behind [`skills_doctor`]).
///
/// When `trusted` the effective global+project catalog is counted; when not, the
/// user-global baseline ONLY (the trust gate omits the project overlay — no
/// project manifest is read or inferred). A discovery failure yields `readable:
/// None` (unreadable/unknown), never a confident `0`.
fn gather_skills(
    config: &localpilot_config::Config,
    cwd: &Path,
    home: Option<&Path>,
    trusted: bool,
) -> SkillsDoctor {
    let readable = match localpilot_skills::discover(cwd, home, trusted) {
        Ok(set) => Some(SkillsCounts {
            discoverable: set.discoverable().count(),
            user_only: set.user_only_count(),
            skipped: set.skipped().len(),
        }),
        Err(_) => None,
    };
    SkillsDoctor {
        autonomous_discovery: config.skills.autonomous_discovery,
        readable,
    }
}

/// The installed-skill-package state for the cwd project, resolving the real
/// per-user home once and deriving trust from the doctor's own trust evaluation
/// (only a `Trusted` workspace reads the project overlay). Best-effort and
/// read-only, like the rest of `doctor`.
fn skills_doctor(trust: TrustState) -> SkillsDoctor {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let config = localpilot_config::load(&ConfigPaths::standard(&cwd), &CliOverrides::default())
        .unwrap_or_default();
    let trusted = matches!(trust, TrustState::Trusted);
    gather_skills(
        &config,
        &cwd,
        localpilot_skills::user_home().as_deref(),
        trusted,
    )
}

/// Resolve the definitions visible from the cwd. Best-effort and read-only.
fn agents() -> AgentsStatus {
    let Ok(cwd) = std::env::current_dir() else {
        return AgentsStatus::default();
    };
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from);
    let set = localpilot_agents::AgentSet::resolve(&localpilot_agents::AgentSet::standard_roots(
        &cwd,
        home.as_deref(),
    ));
    AgentsStatus {
        resolved: set.agents().len(),
        shadowed: set.shadowed().len(),
        errors: set
            .errors()
            .iter()
            .map(|e| format!("{}: {}", e.path.display(), e.reason))
            .collect(),
    }
}

/// Map the result-returning trust query for `cwd` against `store` to a report
/// state and the store path. An unreadable store or an unresolvable path reports
/// `Unknown` — never a confident `Untrusted` from a broken store.
fn workspace_trust_in(cwd: &Path, store: &Path) -> (TrustState, Option<String>) {
    let state = match crate::trust::is_trusted_result_in(cwd, store) {
        Ok(crate::trust::Trust::Trusted) => TrustState::Trusted,
        Ok(crate::trust::Trust::Untrusted) => TrustState::Untrusted,
        Err(_) => TrustState::Unknown,
    };
    (state, Some(store.display().to_string()))
}

/// The trust state of the current directory and the store it was evaluated
/// against.
fn workspace_trust() -> (TrustState, Option<String>) {
    match (std::env::current_dir(), crate::trust::store_path()) {
        (Ok(cwd), Some(store)) => workspace_trust_in(&cwd, &store),
        (_, store) => (
            TrustState::Unknown,
            store.map(|path| path.display().to_string()),
        ),
    }
}

pub fn report() -> DoctorReport {
    let (workspace_trust, workspace_trust_store) = workspace_trust();
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
        agents: agents(),
        capabilities: capabilities(),
        skills: skills_doctor(workspace_trust),
        workspace_trust,
        workspace_trust_store,
        hygiene: None,
    }
}

/// The context-hygiene report for the cwd project: the authored-context layers
/// (instruction files + skills) with their token weights and any advisory
/// findings. Read-only and offline, like the rest of `doctor`. The system-prompt
/// layer is omitted here — it needs the live tool registry (the internal sweep
/// builds it); this reports the user-editable layers.
fn hygiene_report() -> Option<localpilot_contextcheck::ContextReport> {
    let cwd = std::env::current_dir().ok()?;
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let inventory = localpilot_contextcheck::inventory(&cwd, home.as_deref(), true, None);
    Some(localpilot_contextcheck::analyze(
        &inventory,
        &localpilot_contextcheck::Thresholds::default(),
    ))
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
        "trust-cli".to_string(),
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
    run_with(out, crate::output::OutputFormat::Human, false).await
}

/// Gather a report and write it in the requested format. The JSON form is the
/// agent-consumable surface (ADR-0048's `--format` contract extended to `doctor`);
/// the human form is unchanged for an interactive caller.
///
/// # Errors
/// Returns any error from writing to `out`.
pub async fn run_with(
    out: &mut dyn Write,
    format: crate::output::OutputFormat,
    include_context: bool,
) -> io::Result<()> {
    let mut report = report_with_mcp().await;
    if include_context {
        report.hygiene = hygiene_report();
    }
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

    let _ = writeln!(s, "agents:");
    let _ = writeln!(
        s,
        "  definitions: {} resolved, {} shadowed",
        report.agents.resolved, report.agents.shadowed
    );
    if report.agents.errors.is_empty() {
        let _ = writeln!(s, "  load errors: none");
    } else {
        let _ = writeln!(s, "  load errors:");
        for error in &report.agents.errors {
            let _ = writeln!(s, "    {error}");
        }
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
        // Variable *names* are diagnostics; values never appear here or in the
        // JSON rendering. The point of naming them is to say which one to fix.
        let env = if server.env_names.is_empty() {
            String::new()
        } else {
            format!("; env: {}", server.env_names.join(", "))
        };
        if server.state == McpServerState::Connected {
            let protocol = server
                .protocol_version
                .as_deref()
                .map(|version| format!("; protocol {version}"))
                .unwrap_or_default();
            let tools = summarize_mcp_tools(&server.tools);
            let _ = writeln!(
                s,
                "  {} ({}): {}{protocol}; {} tool(s): {tools} ({args}; {command_state}{env})",
                server.name,
                server.command,
                server.state.label(),
                server.tool_count
            );
        } else {
            let error = server.error.as_deref().unwrap_or("unknown error");
            let _ = writeln!(
                s,
                "  {} ({}): {}; {error} ({args}; {command_state}{env})",
                server.name,
                server.command,
                server.state.label()
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

    // Doctor now evaluates trust immediately, so the line is state-accurate.
    let trust = match report.workspace_trust {
        TrustState::Trusted => "trusted",
        TrustState::Untrusted => "not trusted",
        TrustState::Unknown => "could not be evaluated",
    };
    let _ = writeln!(s, "workspace trust: {trust}");
    match report.workspace_trust {
        TrustState::Trusted => {
            if let Some(store) = &report.workspace_trust_store {
                let _ = writeln!(s, "  store: {store}");
            }
        }
        TrustState::Untrusted => {
            if let Some(store) = &report.workspace_trust_store {
                let _ = writeln!(s, "  store: {store}  (grant with `localpilot trust add`)");
            }
        }
        TrustState::Unknown => {
            let _ = writeln!(
                s,
                "  evaluation failed; run `localpilot trust status` for the error"
            );
        }
    }

    // Installed skill packages — a read of stored values only; no scan here.
    let _ = writeln!(s);
    let _ = writeln!(s, "skills:");
    if report.skills.autonomous_discovery {
        let _ = writeln!(s, "  autonomous discovery: on");
    } else {
        let _ = writeln!(
            s,
            "  autonomous discovery: off (set `[skills] autonomous_discovery = true`)"
        );
    }
    match &report.skills.readable {
        Some(counts) => {
            let _ = writeln!(s, "  discoverable packages: {}", counts.discoverable);
            let _ = writeln!(s, "  user-only packages (hidden): {}", counts.user_only);
            // A nonzero skipped count means a `0` above is not a clean empty
            // catalog — some entries were malformed/unreadable (count only).
            if counts.skipped > 0 {
                let _ = writeln!(
                    s,
                    "  package entries skipped as unreadable: {}",
                    counts.skipped
                );
            }
        }
        None => {
            let _ = writeln!(
                s,
                "  installed packages: unreadable (could not scan the skill catalog)"
            );
        }
    }
    // Overlay inclusion is derived from the report's one trust authority, so the
    // skills block never contradicts the trust block. The wording keeps an
    // unevaluable workspace distinct from a confidently untrusted one.
    match report.workspace_trust {
        TrustState::Trusted => {}
        TrustState::Untrusted => {
            let _ = writeln!(s, "  project overlay hidden (workspace untrusted)");
        }
        TrustState::Unknown => {
            let _ = writeln!(
                s,
                "  project overlay hidden (workspace trust could not be evaluated)"
            );
        }
    }
    let _ = writeln!(s);

    if let Some(context) = &report.hygiene {
        render_hygiene(&mut s, context);
    }

    s
}

/// Append the context-hygiene section: per-layer token weights and any advisory
/// findings, most-severe first. Only called when a `--hygiene` report exists, so
/// the default `doctor` rendering is unchanged.
fn render_hygiene(s: &mut String, context: &localpilot_contextcheck::ContextReport) {
    use std::fmt::Write as _;
    let _ = writeln!(s);
    let _ = writeln!(s, "context hygiene:");
    for layer in &context.summary.layers {
        let _ = writeln!(s, "  {}: {} tokens", layer.source, layer.tokens);
    }
    let _ = writeln!(
        s,
        "  total: {} tokens across {} layer(s)",
        context.summary.total_tokens,
        context.summary.layers.len()
    );
    if context.findings.is_empty() {
        let _ = writeln!(s, "  findings: none");
    } else {
        for finding in &context.findings {
            let _ = writeln!(
                s,
                "  [{:?}] {:?}: {}",
                finding.severity, finding.kind, finding.message
            );
        }
    }
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
    let store = localpilot_config::CredentialStore::user();
    for (name, server) in &config.mcp.servers {
        statuses.push(probe_mcp_server(name, server, &store).await);
    }
    statuses
}

fn resolved_config() -> Option<localpilot_config::Config> {
    let cwd = std::env::current_dir().ok()?;
    localpilot_config::load(&ConfigPaths::standard(&cwd), &CliOverrides::default()).ok()
}

async fn probe_mcp_server(
    name: &str,
    server: &localpilot_config::McpServerConfig,
    store: &localpilot_config::CredentialStore,
) -> McpServerStatus {
    let command = server.command.as_str();
    let command_available = command_available(command);
    let mut status = McpServerStatus {
        name: name.to_string(),
        command: command.to_string(),
        arg_count: server.args.len(),
        command_available,
        connected: false,
        protocol_version: None,
        tool_count: 0,
        tools: Vec::new(),
        env_names: Vec::new(),
        state: McpServerState::CommandUnavailable,
        error: None,
    };

    if !command_available {
        status.error = Some(McpServerState::CommandUnavailable.label().to_string());
        return status;
    }

    // Launch through the same seam a live session uses, so `doctor` cannot report
    // a server as healthy that the session would refuse to start (or the
    // reverse). Names are safe to report; values never leave the overlay.
    let (transport, environment) = match crate::mcp_env::spawn_server(server, store) {
        Ok(launched) => launched,
        Err(error) => {
            // A resolution failure knows nothing about the overlay, so fall back
            // to the configured names — the point of reporting them is to say
            // which variable needs attention. The error text names the variable
            // and the credential alias, never a value.
            status.env_names = server.env.keys().cloned().collect();
            status.state = match error {
                crate::mcp_env::ServerLaunchError::Environment(_) => {
                    McpServerState::CredentialMissing
                }
                crate::mcp_env::ServerLaunchError::Transport(_) => McpServerState::StartupFailed,
            };
            status.error = Some(redact(&error.to_string()));
            return status;
        }
    };
    status.env_names = environment.names().map(str::to_string).collect();

    let probe = async {
        let client = McpClient::new(Arc::clone(&transport));
        let server_status = client.initialize().await?;
        let tools = client.list_tools().await?;
        Ok::<_, localpilot_mcp::McpError>((server_status, tools))
    };

    match tokio::time::timeout(MCP_DOCTOR_TIMEOUT, probe).await {
        Ok(Ok((server_status, tools))) => {
            status.state = McpServerState::Connected;
            status.connected = true;
            status.protocol_version = Some(server_status.protocol_version);
            status.tool_count = tools.len();
            status.tools = tools.into_iter().map(|tool| tool.name).collect();
        }
        Ok(Err(error)) => {
            status.state = McpServerState::StartupFailed;
            status.error = Some(redact(&error.to_string()));
        }
        Err(_) => {
            status.state = McpServerState::StartupFailed;
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
            agents: AgentsStatus::default(),
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
                env_names: Vec::new(),
                state: McpServerState::Connected,
                tools: vec!["get-library-docs".to_string()],
                error: None,
            }],
            workspace_trust: TrustState::Unknown,
            workspace_trust_store: Some(
                "/home/user/.config/localpilot/trusted-folders.txt".to_string(),
            ),
            skills: SkillsDoctor {
                autonomous_discovery: false,
                readable: Some(SkillsCounts {
                    discoverable: 2,
                    user_only: 1,
                    skipped: 0,
                }),
            },
            hygiene: None,
        }
    }

    #[test]
    fn render_is_stable() {
        insta::assert_snapshot!(render(&fixture()));
    }

    #[test]
    fn capabilities_advertise_the_trust_cli_surface() {
        assert!(capabilities().contains(&"trust-cli".to_string()));
    }

    #[test]
    fn workspace_trust_maps_state_and_flags_a_broken_store() {
        let cwd = tempfile::tempdir().expect("cwd");
        let home = tempfile::tempdir().expect("home");
        let store = home.path().join("trusted-folders.txt");

        let (state, path) = workspace_trust_in(cwd.path(), &store);
        assert_eq!(state, TrustState::Untrusted);
        assert_eq!(path, Some(store.display().to_string()));

        crate::trust::add_in(cwd.path(), &store).expect("add");
        assert_eq!(
            workspace_trust_in(cwd.path(), &store).0,
            TrustState::Trusted
        );

        // An unreadable store (a directory where the file is expected) is Unknown,
        // not a confident Untrusted.
        let unreadable = home.path().join("store-as-dir");
        std::fs::create_dir_all(&unreadable).expect("mkdir");
        assert_eq!(
            workspace_trust_in(cwd.path(), &unreadable).0,
            TrustState::Unknown
        );
    }

    /// Write a `SKILL.md` package under `skills_dir/<name>/`. `user_only` marks it
    /// `disable-model-invocation: true`.
    fn write_pkg(skills_dir: &Path, name: &str, user_only: bool) {
        let dir = skills_dir.join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let flag = if user_only {
            "disable-model-invocation: true\n"
        } else {
            ""
        };
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: helper {name}\n{flag}---\n\nbody\n"),
        )
        .expect("write skill");
    }

    /// Write a malformed `SKILL.md` (an invalid manifest name) under
    /// `skills_dir/<dir_name>/` — the loader skips it and records it in `skipped`.
    fn write_bad_pkg(skills_dir: &Path, dir_name: &str) {
        let dir = skills_dir.join(dir_name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: Not Valid\ndescription: x\n---\nbody\n",
        )
        .expect("write bad skill");
    }

    fn global_skills(home: &Path) -> PathBuf {
        home.join(".localpilot").join("skills")
    }
    fn project_skills(cwd: &Path) -> PathBuf {
        cwd.join(".localpilot").join("skills")
    }

    #[test]
    fn gather_skills_untrusted_counts_the_global_baseline_only() {
        let cwd = tempfile::tempdir().expect("cwd");
        let home = tempfile::tempdir().expect("home");
        write_pkg(&global_skills(home.path()), "glob-disc", false);
        write_pkg(&global_skills(home.path()), "glob-hidden", true);
        // A project Discoverable + UserOnly that must contribute NOTHING untrusted.
        write_pkg(&project_skills(cwd.path()), "proj-disc", false);
        write_pkg(&project_skills(cwd.path()), "proj-hidden", true);

        let config = localpilot_config::Config::default();
        let s = gather_skills(&config, cwd.path(), Some(home.path()), false);
        assert_eq!(
            s.readable,
            Some(SkillsCounts {
                discoverable: 1,
                user_only: 1,
                skipped: 0
            }),
            "only the global baseline is counted; no project read"
        );
    }

    #[test]
    fn gather_skills_trusted_counts_the_effective_global_plus_project() {
        let cwd = tempfile::tempdir().expect("cwd");
        let home = tempfile::tempdir().expect("home");
        write_pkg(&global_skills(home.path()), "glob-disc", false);
        write_pkg(&global_skills(home.path()), "glob-hidden", true);
        write_pkg(&project_skills(cwd.path()), "proj-disc", false);
        write_pkg(&project_skills(cwd.path()), "proj-hidden", true);

        let config = localpilot_config::Config::default();
        let s = gather_skills(&config, cwd.path(), Some(home.path()), true);
        assert_eq!(
            s.readable,
            Some(SkillsCounts {
                discoverable: 2,
                user_only: 2,
                skipped: 0
            }),
            "the effective global+project catalog is counted"
        );
    }

    #[test]
    fn gather_skills_trusted_respects_a_project_shadowing_a_global_name() {
        let cwd = tempfile::tempdir().expect("cwd");
        let home = tempfile::tempdir().expect("home");
        // Same manifest name in both scopes resolves to ONE effective skill.
        write_pkg(&global_skills(home.path()), "shared", false);
        write_pkg(&project_skills(cwd.path()), "shared", false);

        let config = localpilot_config::Config::default();
        let s = gather_skills(&config, cwd.path(), Some(home.path()), true);
        assert_eq!(
            s.readable,
            Some(SkillsCounts {
                discoverable: 1,
                user_only: 0,
                skipped: 0
            }),
            "a shadowed name is one effective skill, not two"
        );
    }

    #[test]
    fn gather_skills_counts_skipped_entries_alongside_valid_ones() {
        let cwd = tempfile::tempdir().expect("cwd");
        let home = tempfile::tempdir().expect("home");
        write_pkg(&global_skills(home.path()), "glob-disc", false);
        write_bad_pkg(&global_skills(home.path()), "glob-broken");

        let config = localpilot_config::Config::default();
        let s = gather_skills(&config, cwd.path(), Some(home.path()), false);
        assert_eq!(
            s.readable,
            Some(SkillsCounts {
                discoverable: 1,
                user_only: 0,
                skipped: 1
            }),
            "valid counts plus a skipped diagnostic"
        );
    }

    #[test]
    fn gather_skills_all_malformed_is_not_a_clean_empty_catalog() {
        let cwd = tempfile::tempdir().expect("cwd");
        let home = tempfile::tempdir().expect("home");
        write_bad_pkg(&global_skills(home.path()), "glob-broken");

        let config = localpilot_config::Config::default();
        let s = gather_skills(&config, cwd.path(), Some(home.path()), false);
        // Zero real packages, but a nonzero skipped count — NOT a clean empty.
        assert_eq!(
            s.readable,
            Some(SkillsCounts {
                discoverable: 0,
                user_only: 0,
                skipped: 1
            })
        );
    }

    #[test]
    fn gather_skills_genuinely_empty_has_no_skipped() {
        let cwd = tempfile::tempdir().expect("cwd");
        let home = tempfile::tempdir().expect("home");
        // No skill directories at all under either scope.
        let config = localpilot_config::Config::default();
        let s = gather_skills(&config, cwd.path(), Some(home.path()), true);
        assert_eq!(
            s.readable,
            Some(SkillsCounts::default()),
            "a real empty 0/0/0"
        );
    }

    #[test]
    fn gather_skills_autonomous_toggle_changes_only_the_flag() {
        let cwd = tempfile::tempdir().expect("cwd");
        let home = tempfile::tempdir().expect("home");
        write_pkg(&global_skills(home.path()), "glob-disc", false);

        let mut config = localpilot_config::Config::default();
        let off = gather_skills(&config, cwd.path(), Some(home.path()), false);
        config.skills.autonomous_discovery = true;
        let on = gather_skills(&config, cwd.path(), Some(home.path()), false);
        assert_eq!(
            off.readable, on.readable,
            "the flag never changes the counts"
        );
        assert!(!off.autonomous_discovery);
        assert!(on.autonomous_discovery);
    }

    #[test]
    fn render_distinguishes_an_unreadable_catalog_from_an_empty_one() {
        let mut unreadable = fixture();
        unreadable.skills = SkillsDoctor {
            autonomous_discovery: false,
            readable: None,
        };
        let out = render(&unreadable);
        assert!(
            out.contains("installed packages: unreadable"),
            "unreadable must be explicit: {out}"
        );
        assert!(
            !out.contains("discoverable packages:"),
            "an unreadable catalog must not print a count"
        );

        // A genuinely empty catalog: a real 0/0 with no skipped entries.
        let mut empty = fixture();
        empty.skills = SkillsDoctor {
            autonomous_discovery: false,
            readable: Some(SkillsCounts::default()),
        };
        let out = render(&empty);
        assert!(
            out.contains("discoverable packages: 0"),
            "an empty catalog is a real 0: {out}"
        );
        assert!(
            !out.contains("unreadable"),
            "an empty catalog is not unreadable"
        );
        assert!(
            !out.contains("skipped as unreadable"),
            "a clean empty catalog has no skipped line"
        );

        // An all-malformed catalog: 0/0 counts but a nonzero skipped line, so it
        // is NOT presented as a clean empty catalog.
        let mut malformed = fixture();
        malformed.skills = SkillsDoctor {
            autonomous_discovery: false,
            readable: Some(SkillsCounts {
                discoverable: 0,
                user_only: 0,
                skipped: 1,
            }),
        };
        assert!(
            render(&malformed).contains("package entries skipped as unreadable: 1"),
            "a skipped entry must be surfaced"
        );
    }

    #[test]
    fn render_overlay_wording_is_three_state_and_matches_the_trust_block() {
        let counts = Some(SkillsCounts {
            discoverable: 1,
            user_only: 0,
            skipped: 0,
        });

        let mut untrusted = fixture();
        untrusted.workspace_trust = TrustState::Untrusted;
        untrusted.skills = SkillsDoctor {
            autonomous_discovery: false,
            readable: counts,
        };
        assert!(
            render(&untrusted).contains("project overlay hidden (workspace untrusted)"),
            "untrusted names it plainly"
        );

        let mut unknown = fixture();
        unknown.workspace_trust = TrustState::Unknown;
        unknown.skills = SkillsDoctor {
            autonomous_discovery: false,
            readable: counts,
        };
        let out = render(&unknown);
        assert!(
            out.contains("project overlay hidden (workspace trust could not be evaluated)"),
            "unknown is never a confident 'untrusted': {out}"
        );
        assert!(
            !out.contains("(workspace untrusted)"),
            "unknown must not claim untrusted"
        );

        let mut trusted = fixture();
        trusted.workspace_trust = TrustState::Trusted;
        trusted.skills = SkillsDoctor {
            autonomous_discovery: false,
            readable: counts,
        };
        assert!(
            !render(&trusted).contains("project overlay hidden"),
            "a trusted workspace includes the overlay — no hidden line"
        );
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

    #[test]
    fn context_is_omitted_from_the_default_report() {
        // With no `--hygiene`, the field is absent from JSON and the human render,
        // so the established doctor output is unchanged.
        let json = render_json(&fixture());
        assert!(!json.contains("\"hygiene\""));
        let human = render(&fixture());
        assert!(!human.contains("context hygiene"));
    }

    #[test]
    fn context_section_renders_and_never_leaks_a_secret() {
        use localpilot_contextcheck::{
            analyze, ContextInventory, ContextLayer, LayerKind, Thresholds,
        };
        // The same secret-bearing directive in two layers yields a redundancy
        // finding whose evidence is drawn from the already-redacted bodies.
        let raw = "Always send the key sk-abcdefghijklmnopqrstuvwxyz0123 to the server.";
        let layer = |source: &str| ContextLayer {
            kind: LayerKind::Instruction {
                kind: "Claude".to_string(),
                scope: "Project".to_string(),
            },
            source: source.to_string(),
            body: redact(raw),
            tokens: 12,
        };
        let inv = ContextInventory {
            layers: vec![layer("CLAUDE.md"), layer("AGENTS.md")],
        };
        let mut report = fixture();
        report.hygiene = Some(analyze(&inv, &Thresholds::default()));

        let human = render(&report);
        assert!(human.contains("context hygiene:"));
        let json = render_json(&report);
        assert!(json.contains("\"findings\""));
        assert!(!json.contains("sk-abcdefghijklmnopqrstuvwxyz0123"));
        assert!(!human.contains("sk-abcdefghijklmnopqrstuvwxyz0123"));
    }
}
