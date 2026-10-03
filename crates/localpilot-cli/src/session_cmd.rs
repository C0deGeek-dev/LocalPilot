//! `localpilot print` — a non-interactive, single-prompt agent run.
//!
//! Print mode runs the shared session loop once, streams the answer to stdout,
//! and makes no workspace mutations by default: it runs non-interactively, so the
//! permission engine denies write/destructive effects unless writes are
//! explicitly enabled.

use std::io::Write;

use localpilot_config::{CliOverrides, ConfigPaths, StorageConfig};
use localpilot_harness::{RuntimeEvent, SessionConfig, SessionRuntime, StopReason};
use localpilot_llm::ProviderRegistry;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{
    AllowedCommand, Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace,
};
use localpilot_store::{RetentionPolicy, Store};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Every `--permission` value. A flag outside this list is a usage error: an
/// unknown name that quietly became `default` would let a launch meant to be
/// `readonly` write, on any build that predates that profile.
pub const PERMISSION_PROFILES: [&str; 5] =
    ["default", "relaxed", "readonly", "bypass", "unrestricted"];

/// The clap parser every `--permission` flag uses.
#[must_use]
pub fn permission_value_parser() -> clap::builder::PossibleValuesParser {
    clap::builder::PossibleValuesParser::new(PERMISSION_PROFILES)
}

/// Map the `--permission` / `--bypass` flags to a permission profile. `--bypass`
/// wins, and neither `bypass` nor `unrestricted` is ever the default. The flag
/// is validated by [`permission_value_parser`]; a name that still gets here
/// unrecognised fails closed, to `readonly`.
#[must_use]
pub fn resolve_profile(permission: Option<&str>, bypass: bool) -> Profile {
    if bypass {
        return Profile::Bypass;
    }
    match permission {
        None | Some("default") => Profile::Default,
        Some("relaxed") => Profile::Relaxed,
        Some("bypass") => Profile::Bypass,
        Some("unrestricted") => Profile::Unrestricted,
        Some(_) => Profile::ReadOnly,
    }
}

/// The user's exact command list, for the permission engine.
#[must_use]
pub fn allowed_commands(config: &localpilot_config::Config) -> Vec<AllowedCommand> {
    config
        .permissions
        .allow_commands
        .iter()
        .map(|entry| AllowedCommand {
            program: entry.program.clone(),
            args_prefix: entry.args_prefix.clone(),
        })
        .collect()
}

/// Map the configured `[permissions] profile` to a permission profile. The default
/// (no-argument) REPL has no `--permission`/`--bypass` flags to consult, so it reads
/// the profile from config instead of always assuming `Default` — otherwise a
/// project that opted into `profile = "bypass"` would still be prompted per action.
///
/// The sole caller is the `tui`-gated default REPL, so this is dead code in a
/// non-`tui` build; the test below still exercises it under either feature set.
#[must_use]
#[cfg_attr(not(feature = "tui"), allow(dead_code))]

pub fn resolve_profile_from_config(config: &localpilot_config::Config) -> Profile {
    // A run launched with every prompt pre-approved outranks the configured
    // profile. It is a narrower, louder, more deliberate statement than a config
    // file: someone typed it on this run, having been shown what it costs.
    if crate::bypass::engaged() {
        return Profile::Unrestricted;
    }
    match config.permissions.profile {
        localpilot_config::PermissionProfile::Default => Profile::Default,
        localpilot_config::PermissionProfile::Relaxed => Profile::Relaxed,
        localpilot_config::PermissionProfile::Readonly => Profile::ReadOnly,
        localpilot_config::PermissionProfile::Bypass => Profile::Bypass,
        localpilot_config::PermissionProfile::Unrestricted => Profile::Unrestricted,
    }
}

/// The idle window for a served session: `None` when the operator asked for
/// none (`0`), the default otherwise.
///
/// A flag rather than a constant because somebody's workflow will disagree with
/// four hours, and the right answer to that is a knob they can turn — not a
/// server that outlives them.
#[must_use]
pub fn resolve_idle_timeout(minutes: Option<u64>) -> Option<std::time::Duration> {
    match minutes {
        Some(0) => None,
        Some(minutes) => Some(std::time::Duration::from_secs(minutes * 60)),
        None => Some(localpilot_rpc::DEFAULT_MCP_IDLE_TIMEOUT),
    }
}

/// Build the session workspace for `cwd`, granting each configured
/// `[permissions] extra_read_roots` directory standing read scope. A root that
/// cannot be granted (typically: it does not exist) is reported to stderr and
/// skipped, so a stale config entry degrades one grant instead of the session.
pub fn workspace_with_read_roots(
    cwd: &std::path::Path,
    config: &localpilot_config::Config,
) -> Result<Workspace, localpilot_sandbox::SandboxError> {
    let mut workspace = Workspace::new(cwd)?;
    workspace.set_scratch_root(match &config.permissions.scratch_root {
        localpilot_config::ScratchRootConfig::Enabled(true) => {
            localpilot_sandbox::ScratchRoot::OsTemp
        }
        localpilot_config::ScratchRootConfig::Enabled(false) => {
            localpilot_sandbox::ScratchRoot::Disabled
        }
        localpilot_config::ScratchRootConfig::Parent(parent) => {
            localpilot_sandbox::ScratchRoot::Parent(parent.into())
        }
    });
    for root in &config.permissions.extra_read_roots {
        if let Err(error) = workspace.add_read_root(std::path::Path::new(root)) {
            eprintln!("warning: skipping [permissions] extra_read_roots entry {root:?}: {error}");
        }
    }
    Ok(workspace)
}

/// Run print mode for one prompt.
///
/// # Errors
/// Returns an error if configuration, the provider registry, or the workspace
/// cannot be set up.
#[allow(clippy::fn_params_excessive_bools, clippy::too_many_arguments)] // distinct one-shot run toggles
pub async fn print_mode(
    prompt: &str,
    model: &str,
    provider_id: Option<&str>,
    profile: Profile,
    allow_writes: bool,
    self_review: bool,
    resume: Option<localpilot_core::SessionId>,
    turn_timeout_secs: Option<u64>,
    answer_only: bool,
) -> anyhow::Result<PrintOutcome> {
    let cwd = std::env::current_dir()?;
    let mut runtime = build_runtime(&cwd, model, provider_id, profile, allow_writes).await?;
    let config = localpilot_config::load(
        &localpilot_config::ConfigPaths::standard(&cwd),
        &localpilot_config::CliOverrides::default(),
    )?;
    let turn_timeout = print_turn_timeout(turn_timeout_secs, config.harness.turn_timeout_secs);
    runtime.set_turn_timeout(turn_timeout.map(std::time::Duration::from_secs));
    runtime.set_answer_only(answer_only);
    // A turn with no limit that is still going after a while says how to set
    // one, once, on stderr — the answer on stdout is untouched.
    let _slow = turn_timeout
        .is_none()
        .then(|| SlowNotice::after(PRINT_SLOW_AFTER, print_slow_notice(PRINT_SLOW_AFTER)));
    if let Some(session) = resume {
        // Resume rebuilds the conversation from the durable event log; the
        // profile and trust just configured stay in force.
        let report = runtime.load_session(session)?;
        if report.skipped_lines > 0 {
            eprintln!(
                "resume: skipped {} damaged event line(s); continuing with the intact log",
                report.skipped_lines
            );
        }
    }

    let outcome = run_and_print(runtime, prompt).await?;

    // Opt-in advisory cue: a read-only self-review of the workspace after the run.
    // Reuses the existing scanner, writes to stderr (never stdout), and never fails
    // the run — a finished one-shot is not blocked by an advisory pass.
    if self_review {
        let mut err = std::io::stderr();
        if let Err(error) = crate::self_review_cmd::advisory_review(&cwd, &mut err) {
            eprintln!("self-review skipped ({error})");
        }
    }
    Ok(outcome)
}

/// The wall-clock bound of a `print` turn, in seconds.
///
/// `print` is run by a person, or by a caller that sets its own bound, and a
/// local model can need many minutes for one answer — so unlike the other
/// headless paths it takes no built-in bound. `--turn-timeout` wins, then an
/// explicit `[harness] turn_timeout_secs`; a zero from either means no bound.
#[must_use]
pub fn print_turn_timeout(flag: Option<u64>, configured: Option<u64>) -> Option<u64> {
    flag.or(configured).filter(|seconds| *seconds > 0)
}

/// How long a `print` turn with no limit runs before it says how to set one.
pub const PRINT_SLOW_AFTER: std::time::Duration = std::time::Duration::from_secs(120);

/// What a slow, unbounded `print` turn tells the person waiting.
#[must_use]
pub fn print_slow_notice(after: std::time::Duration) -> String {
    format!(
        "note: this turn has been running for {} min and has no time limit — a local model can \
         take a while, and it will keep going. Press Ctrl+C to stop it.\n\
         To set a limit, pass `--turn-timeout <seconds>`, or put this in `.localpilot.toml`:\n\
         \n    [harness]\n    turn_timeout_secs = 600\n",
        after.as_secs() / 60
    )
}

/// Prints a note to stderr once, if it is still alive after a delay. Dropping
/// it — which finishing the work does — cancels the note.
pub struct SlowNotice(tokio::task::JoinHandle<()>);

impl SlowNotice {
    /// Print `message` to stderr after `delay`, unless dropped first.
    #[must_use]
    pub fn after(delay: std::time::Duration, message: String) -> Self {
        Self::after_then(delay, move || eprintln!("{message}"))
    }

    /// Run `action` after `delay`, unless dropped first.
    fn after_then(delay: std::time::Duration, action: impl FnOnce() + Send + 'static) -> Self {
        Self(tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            action();
        }))
    }
}

impl Drop for SlowNotice {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The terminal state of a `print` run a caller can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PrintOutcome {
    /// The output consumer closed stdout before the run finished — a clean stop,
    /// surfaced so the caller can return a distinct exit code rather than crash.
    pub consumer_gone: bool,
}

/// The outcome of one streamed write to the output sink.
#[derive(Debug)]
enum WriteStatus {
    /// The chunk was written and flushed.
    Ok,
    /// The reader closed the sink — a clean stop, never a panic.
    ConsumerGone,
    /// A genuine IO fault, surfaced to stderr by the caller.
    Failed(std::io::Error),
}

/// Write one streamed chunk and flush, classifying the result so a closed reader
/// is a clean stop rather than the process panic the bare `print!` macros take.
/// Takes `&mut dyn Write` so the classification is testable against an injected
/// broken-pipe sink.
fn write_streamed(out: &mut dyn std::io::Write, text: &str) -> WriteStatus {
    match write!(out, "{text}").and_then(|()| out.flush()) {
        Ok(()) => WriteStatus::Ok,
        Err(error) if output_consumer_gone(&error) => WriteStatus::ConsumerGone,
        Err(error) => WriteStatus::Failed(error),
    }
}

/// Whether an stdout write error means the output consumer went away (the reader
/// closed the pipe) rather than a genuine IO fault. A consumer-gone write is a
/// clean stop, not a panic; any other IO error is still surfaced.
#[must_use]
pub fn output_consumer_gone(err: &std::io::Error) -> bool {
    if err.kind() == std::io::ErrorKind::BrokenPipe {
        return true;
    }
    // Windows surfaces a closed read end as ERROR_BROKEN_PIPE (109) or
    // ERROR_NO_DATA (232) — the latter is the "The pipe is being closed" message
    // the dogfood run hit. Match both raw codes so the classification holds on
    // every tier-1 platform, not only where std maps them to `BrokenPipe`.
    matches!(err.raw_os_error(), Some(109) | Some(232))
}

/// Build a non-interactive session runtime for `cwd` with the configured
/// provider, tools (MCP + broker), and context hook — the shared setup both
/// `print` and `eval` use, so a headless eval run sees the same harness a real
/// run does. `trusted` enables workspace writes.
///
/// # Errors
/// Returns an error if configuration, the provider registry, or the workspace
/// cannot be set up.
pub async fn build_runtime(
    cwd: &std::path::Path,
    model: &str,
    provider_id: Option<&str>,
    profile: Profile,
    trusted: bool,
) -> anyhow::Result<SessionRuntime> {
    build_runtime_with_store(
        cwd,
        model,
        provider_id,
        profile,
        trusted,
        Store::open(cwd),
        true,
    )
    .await
}

/// [`build_runtime`] with the session's store given, and whether configured
/// MCP servers are started. An in-memory store keeps a run from writing its
/// transcript into `cwd`, and no MCP servers keeps its start-up from
/// launching any configured program, as a pair navigator's review turn must.
#[allow(clippy::too_many_arguments)] // the shared print/eval set plus two isolation switches
///
/// # Errors
/// As [`build_runtime`].
pub async fn build_runtime_with_store(
    cwd: &std::path::Path,
    model: &str,
    provider_id: Option<&str>,
    profile: Profile,
    trusted: bool,
    store: Store,
    start_mcp_servers: bool,
) -> anyhow::Result<SessionRuntime> {
    build_runtime_with_store_and_deadline(
        cwd,
        model,
        provider_id,
        profile,
        trusted,
        store,
        start_mcp_servers,
    )
    .await
    .map(|(runtime, _)| runtime)
}

/// Metadata resolved from the same configuration used to build a headless turn.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TurnDeadline {
    pub(crate) seconds: Option<u64>,
    pub(crate) source: &'static str,
}

impl TurnDeadline {
    pub(crate) fn from_config(config: &localpilot_config::Config) -> Self {
        Self {
            seconds: config.harness.resolved_rails(false).turn_timeout_secs,
            source: if config.harness.turn_timeout_secs.is_some() {
                "config"
            } else {
                "builtin"
            },
        }
    }

    pub(crate) fn log_fields(&self) -> String {
        format!(
            "turn_timeout_secs={} turn_timeout_source={}",
            self.seconds
                .map_or_else(|| "none".into(), |s| s.to_string()),
            self.source,
        )
    }
}

#[allow(clippy::too_many_arguments)] // same shared session inputs, with resolved metadata
pub(crate) async fn build_runtime_with_store_and_deadline(
    cwd: &std::path::Path,
    model: &str,
    provider_id: Option<&str>,
    profile: Profile,
    trusted: bool,
    store: Store,
    start_mcp_servers: bool,
) -> anyhow::Result<(SessionRuntime, TurnDeadline)> {
    let config = localpilot_config::load(&ConfigPaths::standard(cwd), &CliOverrides::default())?;
    let registry = ProviderRegistry::from_config(&config)?;
    let provider = match provider_id {
        Some(id) => registry.get(id),
        None => registry.default_provider(),
    }
    .cloned()
    .ok_or_else(|| anyhow::anyhow!("no provider is configured"))?;

    let deadline = TurnDeadline::from_config(&config);
    let runtime = build_runtime_with_provider(
        cwd,
        model,
        profile,
        trusted,
        store,
        start_mcp_servers,
        &config,
        provider,
    )
    .await?;
    Ok((runtime, deadline))
}

#[allow(clippy::too_many_arguments)] // explicit session inputs, with config/provider injectable
async fn build_runtime_with_provider(
    cwd: &std::path::Path,
    model: &str,
    profile: Profile,
    trusted: bool,
    store: Store,
    start_mcp_servers: bool,
    config: &localpilot_config::Config,
    provider: std::sync::Arc<dyn localpilot_llm::ModelProvider>,
) -> anyhow::Result<SessionRuntime> {
    let resolution = crate::context_window::resolve(
        config,
        &provider.declaration().id,
        model,
        provider.declaration().max_context_tokens,
    )
    .await;
    if let Some(warning) = resolution.warning_once() {
        eprintln!("{warning}");
    }
    let context_token_limit = resolution.window.budget(
        config.harness.context_token_limit,
        provider.declaration().max_output_tokens,
    );
    let mcp = if start_mcp_servers {
        crate::mcp::McpTools::load(config).await
    } else {
        crate::mcp::McpTools::without_servers(config)
    };
    let mut registry = mcp.registry();
    let broker = crate::mcp::install_broker(&config.tools, &mut registry);
    // Headless run (print/eval): apply the built-in safety rails so a project
    // with no `[harness]` budget/timeout still self-bounds (ADR-0055). Explicit
    // config values win inside `resolved_rails`.
    let rails = config.harness.resolved_rails(false);
    let mut runtime = SessionRuntime::new(
        provider,
        registry,
        PermissionEngine::new(profile, Vec::new()).with_allowed_commands(allowed_commands(config)),
        Box::new(ScriptedApprover::new(Vec::new())),
        store,
        workspace_with_read_roots(cwd, config)?,
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            model: model.to_string(),
            interactivity: Interactivity::NonInteractive,
            trusted,
            context_token_limit,
            compaction_mode: compaction_mode(config.compaction.mode),
            summarizer_tuning: localpilot_harness::SummarizerTuning::from_config(
                &config.compaction,
            ),
            tool_call_budget: rails.tool_call_budget,
            tool_call_budget_max: rails.tool_call_budget_max,
            tool_budget_explicit: rails.budget_explicit,
            rules: config.harness.rules.clone(),
            enforce_claim_gate: config.harness.claim_gate.is_enabled(),
            tool_marker_enabled: config.tools.marker,
            enforce_readable_errors: config.tools.readable_errors,
            repair_mode: config.tools.repair,
            elide_seen_reads: config.tools.elide_seen_reads,
            turn_timeout: rails.turn_timeout_secs.map(std::time::Duration::from_secs),
            granularity: Some(config.harness.granularity.clone()),
            verify_before_done: config.harness.verify_before_done,
            verify_command: config.harness.verify_command.clone(),
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    tracing::debug!(
        context_budget = runtime.context_usage().1,
        context_source = resolution.window.source.as_str(),
        "headless context resolved"
    );
    runtime.set_broker(broker);
    if let Some(agents) = crate::agents_cmd::session_agents(cwd) {
        runtime.set_agents(agents);
    }
    localpilot_harness::register_project_analysis_context(
        cwd,
        config.context.project_analysis,
        config.docs.lookup_policy,
        &mut runtime,
    );
    localpilot_harness::register_project_instructions_context(
        cwd,
        config.context.inject_instructions,
        config.context.instruction_char_budget,
        &mut runtime,
    );
    localpilot_localmind::register_context_hook(cwd, &mut runtime);
    Ok(runtime)
}

fn compaction_mode(mode: localpilot_config::CompactionMode) -> localpilot_harness::CompactionMode {
    match mode {
        localpilot_config::CompactionMode::Deterministic => {
            localpilot_harness::CompactionMode::Deterministic
        }
        localpilot_config::CompactionMode::SmartWithFallback => {
            localpilot_harness::CompactionMode::SmartWithFallback
        }
    }
}

/// Resolve a session reference — a full session id (UUID) or a conversation name
/// — into a session id, looking a name up in this workspace's index. A session id
/// is a UUID, so a human name can never be mistaken for one.
///
/// # Errors
/// Returns an error if the reference is neither a parseable id nor a known name
/// in this workspace, or if the index cannot be read.
pub fn resolve_session_ref(reference: &str) -> anyhow::Result<localpilot_core::SessionId> {
    let cwd = std::env::current_dir()?;
    resolve_session_ref_in_store(&Store::open(&cwd), reference)
}

pub(crate) fn resolve_session_ref_in_store(
    store: &Store,
    reference: &str,
) -> anyhow::Result<localpilot_core::SessionId> {
    let reference = reference.trim();
    if let Ok(id) = reference.parse::<localpilot_core::SessionId>() {
        return Ok(id);
    }
    let entry = store.find_session_by_name(reference)?.ok_or_else(|| {
        anyhow::anyhow!("no session id or name matches {reference:?} in this workspace")
    })?;
    Ok(entry.id)
}

/// Resolve `--continue` / `--resume <id-or-name>` into a session id.
///
/// # Errors
/// Returns an error for a reference that is neither a valid id nor a known name,
/// or `--continue` with no sessions.
pub fn resolve_resume(
    continue_latest: bool,
    resume: Option<&str>,
) -> anyhow::Result<Option<localpilot_core::SessionId>> {
    if let Some(reference) = resume {
        return Ok(Some(resolve_session_ref(reference)?));
    }
    if !continue_latest {
        return Ok(None);
    }
    let cwd = std::env::current_dir()?;
    let latest = Store::open(&cwd)
        .latest_session()?
        .ok_or_else(|| anyhow::anyhow!("no sessions exist in this workspace yet"))?;
    Ok(Some(latest.id))
}

/// Name (or rename) a session so it can later be resumed by name. `reference` is
/// the session's id or its current name; `name` is the new name.
///
/// # Errors
/// Returns an error if the reference does not resolve, the name is empty or
/// id-shaped, the name is already used by another session, or the index write
/// fails.
pub fn name_session(reference: &str, name: &str) -> anyhow::Result<()> {
    let id = resolve_session_ref(reference)?;
    let cwd = std::env::current_dir()?;
    Store::open(&cwd).set_session_name(id, name)?;
    Ok(())
}

/// Print this workspace's sessions, most recent first.
///
/// # Errors
/// Returns an error if the session index cannot be read or output fails.
pub fn list_sessions(out: &mut impl Write) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let mut sessions = Store::open(&cwd).list_sessions()?;
    sessions.sort_by(|a, b| b.updated_unix.cmp(&a.updated_unix));
    if sessions.is_empty() {
        writeln!(out, "no sessions in this workspace")?;
        return Ok(());
    }
    for entry in sessions {
        writeln!(out, "{}", format_session_line(&entry))?;
    }
    Ok(())
}

/// One session-list line: id, message count, updated time, optional name, and a
/// `[cc-import]` badge for a session imported from Claude Code (its name carries
/// the `imported_cc_` prefix the importer assigns).
fn format_session_line(entry: &localpilot_store::SessionIndexEntry) -> String {
    let name = entry
        .name
        .as_deref()
        .map(|n| format!("  name: {n}"))
        .unwrap_or_default();
    let badge = entry
        .name
        .as_deref()
        .is_some_and(|n| n.starts_with("imported_cc_"))
        .then_some("  [cc-import]")
        .unwrap_or_default();
    format!(
        "{}  messages: {:<4} updated: {}{name}{badge}",
        entry.id, entry.message_count, entry.updated_unix
    )
}

#[cfg(test)]
mod session_line_tests {
    use super::format_session_line;
    use localpilot_core::SessionId;
    use localpilot_store::SessionIndexEntry;

    fn entry(name: Option<&str>) -> SessionIndexEntry {
        SessionIndexEntry {
            id: SessionId::new(),
            message_count: 3,
            created_unix: 0,
            updated_unix: 0,
            name: name.map(str::to_string),
        }
    }

    #[test]
    fn an_imported_session_gets_a_cc_import_badge() {
        assert!(format_session_line(&entry(Some("imported_cc_abc"))).contains("[cc-import]"));
        assert!(!format_session_line(&entry(Some("my-session"))).contains("[cc-import]"));
        assert!(!format_session_line(&entry(None)).contains("[cc-import]"));
    }
}

/// Export a session as an inspectable, redacted bundle.
///
/// # Errors
/// Returns an error for an unparsable id or a store/write failure.
pub fn export_session(id: &str, output: &std::path::Path) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let session: localpilot_core::SessionId = id.parse()?;
    Store::open(&cwd).export_session(session, output)?;
    Ok(())
}

/// Build a retention policy from the configured `[storage]` defaults, overridden
/// per-run by explicit `keep` / `older_than` flags.
#[must_use]
pub fn retention_policy(
    storage: &StorageConfig,
    keep: Option<u64>,
    older_than: Option<u64>,
) -> RetentionPolicy {
    RetentionPolicy {
        max_sessions: keep.unwrap_or(storage.max_sessions),
        max_age_days: older_than.unwrap_or(storage.max_age_days),
    }
}

/// Current wall-clock time as a Unix timestamp (seconds), or `0` if the clock is
/// before the epoch.
#[must_use]
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Prune this workspace's sessions per the retention policy, printing a summary.
///
/// # Errors
/// Returns an error if configuration or the store cannot be read, or a delete
/// fails.
pub fn prune_sessions(
    keep: Option<u64>,
    older_than: Option<u64>,
    dry_run: bool,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let config = localpilot_config::load(&ConfigPaths::standard(&cwd), &CliOverrides::default())?;
    let policy = retention_policy(&config.storage, keep, older_than);

    if policy.is_unbounded() {
        writeln!(out, "no retention limits set — nothing to prune")?;
        return Ok(());
    }

    let report = Store::open(&cwd).prune(policy, now_unix(), dry_run)?;
    let verb = if dry_run { "would remove" } else { "removed" };
    writeln!(
        out,
        "{verb} {} session(s) and {} tool-output snapshot(s)",
        report.sessions_removed, report.tool_outputs_removed
    )?;
    Ok(())
}

/// The bounded stderr line for a runtime event a headless caller should see,
/// or `None` for events that stay silent. Diagnostics only — nothing returned
/// here may reach stdout, which carries the answer and nothing else.
fn diagnostic_line_for(event: &RuntimeEvent) -> Option<String> {
    match event {
        RuntimeEvent::ToolFinished {
            name,
            is_error: true,
            output,
            ..
        } => Some(format!(
            "tool failed: {name}: {}\n",
            failure_preview(output)
        )),
        RuntimeEvent::Warning(warning) => Some(format!("warning: {}\n", failure_preview(warning))),
        RuntimeEvent::ToolStuck { name, count } => Some(format!(
            "stuck: tool `{name}` failed {count} times this turn\n"
        )),
        _ => None,
    }
}

/// Collapse whitespace and cap the preview so a large tool result cannot flood
/// the diagnostics stream. The text was already redacted at the dispatch
/// chokepoint; this bounds, it does not sanitize.
fn failure_preview(text: &str) -> String {
    const MAX_CHARS: usize = 240;
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut preview: String = collapsed.chars().take(MAX_CHARS).collect();
    if collapsed.chars().count() > MAX_CHARS {
        preview.push('…');
    }
    preview
}

/// One step of the print-mode event loop.
enum PrinterStep {
    Event(RuntimeEvent),
    /// The receiver fell behind and `n` events were discarded by the channel.
    /// The stream continues — losing some events must not mean losing the rest.
    Dropped(u64),
    /// The channel is closed: the turn is over.
    End,
}

async fn next_printer_event(rx: &mut broadcast::Receiver<RuntimeEvent>) -> PrinterStep {
    match rx.recv().await {
        Ok(event) => PrinterStep::Event(event),
        Err(broadcast::error::RecvError::Lagged(n)) => PrinterStep::Dropped(n),
        Err(broadcast::error::RecvError::Closed) => PrinterStep::End,
    }
}

async fn run_and_print(mut runtime: SessionRuntime, prompt: &str) -> anyhow::Result<PrintOutcome> {
    let (events, mut rx) = broadcast::channel(1024);
    let cancel = CancellationToken::new();

    // The printer owns stdout. A broken pipe (the reader closed) is a clean stop:
    // it cancels the turn and reports the consumer gone instead of aborting — the
    // bare `println!`/`print!` family panics the process on a write error, which is
    // the forbidden runtime-path panic this code must not take.
    let printer_cancel = cancel.clone();
    let printer = tokio::spawn(async move {
        let mut out = std::io::stdout();
        let mut err = std::io::stderr();
        let mut consumer_gone = false;
        // Two independent "consumer gone" flags: a closed *stdout* cancels the
        // turn (the answer has nowhere to go); a closed *stderr* only silences
        // further diagnostics and is never reported as the consumer going away.
        // Both go through the checked writer — the bare `eprintln!` family
        // panics the process on a write error, the same forbidden runtime-path
        // panic the stdout comment below explains.
        let mut diagnostics_gone = false;
        loop {
            let event = match next_printer_event(&mut rx).await {
                PrinterStep::Event(event) => event,
                PrinterStep::Dropped(missed) => {
                    // Diagnostics only — dropped events must not end the stream.
                    if !diagnostics_gone {
                        let note = format!(
                            "print: fell behind the event stream; {missed} event(s) dropped\n"
                        );
                        diagnostics_gone =
                            !matches!(write_streamed(&mut err, &note), WriteStatus::Ok);
                    }
                    continue;
                }
                PrinterStep::End => break,
            };
            if !diagnostics_gone {
                if let Some(line) = diagnostic_line_for(&event) {
                    diagnostics_gone = !matches!(write_streamed(&mut err, &line), WriteStatus::Ok);
                }
            }
            match event {
                RuntimeEvent::Text(text) => match write_streamed(&mut out, &text) {
                    WriteStatus::Ok => {}
                    WriteStatus::ConsumerGone => {
                        consumer_gone = true;
                        printer_cancel.cancel();
                        break;
                    }
                    WriteStatus::Failed(error) => {
                        // A genuine IO fault still surfaces — but never as a panic.
                        let note = format!("print: failed writing to stdout: {error}\n");
                        let _ = write_streamed(&mut err, &note);
                        break;
                    }
                },
                RuntimeEvent::Stopped(_) => break,
                _ => {}
            }
        }
        consumer_gone
    });

    let reason = runtime.run_turn(prompt, &events, &cancel).await;
    drop(events);
    let mut consumer_gone = printer.await.unwrap_or(false);

    // Terminate the streamed answer with a newline — a checked write, so a reader
    // that closed mid-stream is a clean stop here too, not a panic.
    if !consumer_gone {
        match write_streamed(&mut std::io::stdout(), "\n") {
            WriteStatus::Ok => {}
            WriteStatus::ConsumerGone => consumer_gone = true,
            WriteStatus::Failed(error) => {
                eprintln!("print: failed writing to stdout: {error}");
            }
        }
    }

    // A bounded, parseable terminal handoff on stderr (never stdout, so it can't
    // pollute the answer): a non-interactive caller always reads a terminal state —
    // stop reason, tool calls, files changed, whether memory was written — even
    // when the turn timed out or the consumer went away.
    if let Some(handoff) = runtime.last_turn_handoff() {
        let line = format!("handoff: {}\n", handoff.to_json_line());
        let _ = write_streamed(&mut std::io::stderr(), &line);
    }

    if reason == StopReason::Degraded {
        let _ = write_streamed(
            &mut std::io::stderr(),
            "warning: the model was marked degraded after repeated bad output\n",
        );
    }
    Ok(PrintOutcome { consumer_gone })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slow_notice_names_the_flag_and_shows_the_config_example() {
        let notice = print_slow_notice(PRINT_SLOW_AFTER);
        assert!(notice.contains("running for 2 min"), "{notice}");
        assert!(notice.contains("has no time limit"), "{notice}");
        assert!(notice.contains("--turn-timeout <seconds>"), "{notice}");
        assert!(
            notice.contains("[harness]\n    turn_timeout_secs = 600"),
            "{notice}"
        );
        assert!(notice.contains("Ctrl+C"), "{notice}");
    }

    #[tokio::test]
    async fn a_slow_notice_fires_once_after_its_delay_and_never_after_the_work_ends() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let delay = std::time::Duration::from_millis(200);
        let fired = Arc::new(AtomicUsize::new(0));
        let count = {
            let fired = fired.clone();
            move || {
                fired.fetch_add(1, Ordering::SeqCst);
            }
        };

        let slow = SlowNotice::after_then(delay, count.clone());
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(fired.load(Ordering::SeqCst), 0, "not before the delay");
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert_eq!(
            fired.load(Ordering::SeqCst),
            1,
            "once, however long it runs"
        );
        drop(slow);

        // Work that finishes first cancels its note.
        let quick = SlowNotice::after_then(delay, count);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        drop(quick);
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert_eq!(
            fired.load(Ordering::SeqCst),
            1,
            "a finished turn never prints the note"
        );
    }

    #[test]
    fn a_print_turn_has_no_time_limit_unless_one_is_asked_for() {
        // Neither a flag nor configuration: no bound, where the other headless
        // paths still take the built-in one.
        assert_eq!(print_turn_timeout(None, None), None);
        assert_eq!(
            localpilot_config::Config::default()
                .harness
                .resolved_rails(false)
                .turn_timeout_secs,
            Some(localpilot_config::DEFAULT_HEADLESS_TURN_TIMEOUT_SECS)
        );
        // Configuration bounds it; the flag wins over configuration.
        assert_eq!(print_turn_timeout(None, Some(90)), Some(90));
        assert_eq!(print_turn_timeout(Some(45), Some(90)), Some(45));
        // A zero from either says "no bound" out loud.
        assert_eq!(print_turn_timeout(Some(0), Some(90)), None);
        assert_eq!(print_turn_timeout(None, Some(0)), None);
    }

    #[test]
    fn turn_deadline_metadata_uses_resolved_headless_rails() {
        let mut config = localpilot_config::Config::default();
        assert_eq!(
            TurnDeadline::from_config(&config).log_fields(),
            "turn_timeout_secs=600 turn_timeout_source=builtin"
        );
        for seconds in [0, 43, 900] {
            config.harness.turn_timeout_secs = Some(seconds);
            let deadline = TurnDeadline::from_config(&config);
            assert_eq!(
                deadline.seconds,
                config.harness.resolved_rails(false).turn_timeout_secs
            );
            assert_eq!(deadline.source, "config");
            assert_eq!(
                deadline.log_fields(),
                format!("turn_timeout_secs={seconds} turn_timeout_source=config")
            );
        }
    }

    #[tokio::test]
    async fn context_headless_runtime_uses_server_window_instead_of_default_budget() {
        use localpilot_llm::{FakeProvider, ModelProvider};
        use wiremock::{
            matchers::{method, path},
            Mock, MockServer, ResponseTemplate,
        };

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/props"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "default_generation_settings":{"n_ctx":262144}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let base = format!("{}/v1", server.uri());
        let config = crate::context_window::tests::config_for(&base, None);
        crate::context_window::resolve_with_probe(
            &config,
            "context-test",
            "model",
            None,
            localpilot_llm::probe_context_window(&base, "model", None, true),
        )
        .await;
        let mut declaration = FakeProvider::new().declaration().clone();
        declaration.id = "context-test".to_owned();
        declaration.max_context_tokens = None;
        declaration.max_output_tokens = Some(4096);
        let provider = std::sync::Arc::new(FakeProvider::new().with_declaration(declaration));
        let cwd = tempfile::tempdir().unwrap();
        let runtime = build_runtime_with_provider(
            cwd.path(),
            "model",
            Profile::Default,
            true,
            Store::ephemeral(),
            false,
            &config,
            provider,
        )
        .await
        .unwrap();
        assert_eq!(runtime.context_usage().1, 262_144 - 4096);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[test]
    fn failing_tool_events_map_to_bounded_stderr_lines() {
        // A failing tool names itself on the diagnostics stream...
        let line = diagnostic_line_for(&RuntimeEvent::ToolFinished {
            id: "c1".to_string(),
            name: "run_shell".to_string(),
            is_error: true,
            cancelled: false,
            output: "exit: 1\n--- stdout ---\n\n--- stderr ---\nboom".to_string(),
            duration_ms: 0,
        })
        .expect("a failing tool is diagnosed");
        assert!(line.starts_with("tool failed: run_shell:"));
        assert!(!line.trim_end_matches('\n').contains('\n'), "one line only");

        // ...a successful one stays silent...
        assert!(diagnostic_line_for(&RuntimeEvent::ToolFinished {
            id: "c2".to_string(),
            name: "run_shell".to_string(),
            is_error: false,
            cancelled: false,
            output: "exit: 0".to_string(),
            duration_ms: 0,
        })
        .is_none());

        // ...and answer text never becomes a diagnostic.
        assert!(diagnostic_line_for(&RuntimeEvent::Text("the answer".to_string())).is_none());
    }

    #[test]
    fn a_huge_failing_output_is_capped_in_the_preview() {
        let huge = "x".repeat(64 * 1024);
        let line = diagnostic_line_for(&RuntimeEvent::ToolFinished {
            id: "c1".to_string(),
            name: "run_shell".to_string(),
            is_error: true,
            cancelled: false,
            output: huge,
            duration_ms: 0,
        })
        .expect("a failing tool is diagnosed");
        assert!(
            line.len() < 1024,
            "unbounded stderr line: {} bytes",
            line.len()
        );
        assert!(line.contains('…'), "the cap is explicit");
    }

    #[tokio::test]
    async fn lagged_printer_receiver_keeps_reading() {
        // A slow consumer must skip the dropped events and keep printing;
        // only a closed channel ends the loop.
        let (tx, mut rx) = broadcast::channel(2);
        for i in 0..5 {
            tx.send(RuntimeEvent::Text(format!("chunk {i}")))
                .expect("receiver alive");
        }

        let PrinterStep::Dropped(missed) = next_printer_event(&mut rx).await else {
            panic!("an overrun receiver reports the drop, not end-of-stream");
        };
        assert_eq!(missed, 3);

        // The surviving tail of the stream is still delivered after the lag.
        let PrinterStep::Event(RuntimeEvent::Text(text)) = next_printer_event(&mut rx).await else {
            panic!("the stream continues after a lag");
        };
        assert_eq!(text, "chunk 3");

        drop(tx);
        let PrinterStep::Event(_) = next_printer_event(&mut rx).await else {
            panic!("buffered events drain before close");
        };
        assert!(matches!(
            next_printer_event(&mut rx).await,
            PrinterStep::End
        ));
    }

    #[test]
    fn flags_map_to_profiles() {
        assert_eq!(resolve_profile(None, false), Profile::Default);
        assert_eq!(resolve_profile(Some("relaxed"), false), Profile::Relaxed);
        assert_eq!(resolve_profile(Some("default"), false), Profile::Default);
        assert_eq!(
            resolve_profile(Some("unrestricted"), false),
            Profile::Unrestricted
        );
        // --bypass always wins and is explicit.
        assert_eq!(resolve_profile(None, true), Profile::Bypass);
        assert_eq!(resolve_profile(Some("relaxed"), true), Profile::Bypass);
        assert_eq!(resolve_profile(Some("bypass"), false), Profile::Bypass);
        assert_eq!(resolve_profile(Some("readonly"), false), Profile::ReadOnly);
        // Every accepted name maps to its own profile; a name that slipped
        // past validation fails closed, never to `default`.
        for name in PERMISSION_PROFILES {
            assert_eq!(profile_name(resolve_profile(Some(name), false)), name);
        }
        assert_eq!(resolve_profile(Some("read-only"), false), Profile::ReadOnly);
    }

    fn profile_name(profile: Profile) -> &'static str {
        crate::server_cmd::profile_label(profile)
    }

    #[test]
    fn config_profile_maps_to_permission_profile() {
        // The default REPL reads its profile from config; a project that set
        // `profile = "bypass"` must actually run bypassed, not fall back to Default.
        let mut config = localpilot_config::Config::default();
        config.permissions.profile = localpilot_config::PermissionProfile::Default;
        assert_eq!(resolve_profile_from_config(&config), Profile::Default);
        config.permissions.profile = localpilot_config::PermissionProfile::Relaxed;
        assert_eq!(resolve_profile_from_config(&config), Profile::Relaxed);
        config.permissions.profile = localpilot_config::PermissionProfile::Bypass;
        assert_eq!(resolve_profile_from_config(&config), Profile::Bypass);
        config.permissions.profile = localpilot_config::PermissionProfile::Readonly;
        assert_eq!(resolve_profile_from_config(&config), Profile::ReadOnly);
        config.permissions.profile = localpilot_config::PermissionProfile::Unrestricted;
        assert_eq!(resolve_profile_from_config(&config), Profile::Unrestricted);
    }

    #[test]
    fn store_resolver_accepts_trimmed_ids_and_case_insensitive_names() {
        let root = tempfile::tempdir().expect("temp workspace");
        let store = Store::open(root.path());
        let session = localpilot_core::SessionId::new();
        store
            .set_session_name(session, "Named Conversation")
            .expect("name session");

        assert_eq!(
            resolve_session_ref_in_store(&store, &format!("  {session}  ")).expect("id"),
            session
        );
        assert_eq!(
            resolve_session_ref_in_store(&store, "  named conversation  ").expect("name"),
            session
        );
    }

    #[test]
    fn workspace_with_read_roots_grants_configured_roots_and_skips_missing_ones() {
        let cwd = tempfile::tempdir().unwrap();
        let granted = tempfile::tempdir().unwrap();
        std::fs::write(granted.path().join("note.md"), "x").unwrap();

        let mut config = localpilot_config::Config::default();
        config.permissions.extra_read_roots = vec![
            granted.path().display().to_string(),
            // A stale entry must degrade to a skipped grant, not a failed session.
            granted.path().join("no-such-dir").display().to_string(),
        ];

        let workspace = workspace_with_read_roots(cwd.path(), &config).unwrap();
        assert!(workspace.read_scoped(&granted.path().join("note.md")));
        assert!(!workspace.contains(granted.path()));
    }

    #[test]
    fn host_workspace_applies_disabled_and_custom_scratch_configuration() {
        let cwd = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let mut config = localpilot_config::Config::default();
        config.permissions.scratch_root = localpilot_config::ScratchRootConfig::Enabled(false);
        let mut workspace = workspace_with_read_roots(cwd.path(), &config).unwrap();
        workspace.start_scratch("disabled").unwrap();
        assert!(workspace.scratch_dir().is_none());
        config.permissions.scratch_root =
            localpilot_config::ScratchRootConfig::Parent(parent.path().display().to_string());
        let mut workspace = workspace_with_read_roots(cwd.path(), &config).unwrap();
        workspace.start_scratch("custom").unwrap();
        let scratch = workspace.scratch_dir().unwrap().to_path_buf();
        assert!(scratch.starts_with(std::fs::canonicalize(parent.path()).unwrap()));
        assert!(!workspace.scratch_contains(parent.path()));
        workspace.clear_scratch();
        assert!(!scratch.exists());
        assert!(parent.path().exists());
    }

    /// A sink whose every write fails with the given pipe-closed error, standing in
    /// for a reader that closed stdout mid-stream.
    struct BrokenPipeSink(std::io::Error);

    impl std::io::Write for BrokenPipeSink {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(self.0.kind(), "closed"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::new(self.0.kind(), "closed"))
        }
    }

    #[test]
    fn a_closed_reader_is_classified_as_consumer_gone() {
        let broken = std::io::Error::from(std::io::ErrorKind::BrokenPipe);
        assert!(output_consumer_gone(&broken));
        // Windows surfaces the closed read end as ERROR_BROKEN_PIPE (109) or
        // ERROR_NO_DATA (232 — "The pipe is being closed"); both are consumer-gone.
        assert!(output_consumer_gone(&std::io::Error::from_raw_os_error(
            109
        )));
        assert!(output_consumer_gone(&std::io::Error::from_raw_os_error(
            232
        )));
    }

    #[test]
    fn a_real_io_fault_is_not_consumer_gone() {
        assert!(!output_consumer_gone(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
        assert!(!output_consumer_gone(&std::io::Error::from(
            std::io::ErrorKind::NotFound
        )));
    }

    #[test]
    fn streaming_to_a_closed_reader_stops_cleanly_without_panicking() {
        // The regression: the bare `print!`/`println!` macros panic the process on
        // a broken pipe. The checked write path reports the consumer gone instead.
        let mut sink = BrokenPipeSink(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
        assert!(matches!(
            write_streamed(&mut sink, "streamed answer"),
            WriteStatus::ConsumerGone
        ));
    }

    #[test]
    fn streaming_to_a_healthy_sink_succeeds() {
        let mut buf: Vec<u8> = Vec::new();
        assert!(matches!(write_streamed(&mut buf, "hello"), WriteStatus::Ok));
        assert_eq!(buf, b"hello");
    }
}
