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
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::{RetentionPolicy, Store};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Map the `--permission` / `--bypass` flags to a permission profile. `--bypass`
/// wins, and neither `bypass` nor `unrestricted` is ever the default.
#[must_use]
pub fn resolve_profile(permission: Option<&str>, bypass: bool) -> Profile {
    if bypass {
        return Profile::Bypass;
    }
    match permission {
        Some("relaxed") => Profile::Relaxed,
        Some("bypass") => Profile::Bypass,
        Some("unrestricted") => Profile::Unrestricted,
        _ => Profile::Default,
    }
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
    match config.permissions.profile {
        localpilot_config::PermissionProfile::Default => Profile::Default,
        localpilot_config::PermissionProfile::Relaxed => Profile::Relaxed,
        localpilot_config::PermissionProfile::Bypass => Profile::Bypass,
        localpilot_config::PermissionProfile::Unrestricted => Profile::Unrestricted,
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
#[allow(clippy::fn_params_excessive_bools)] // distinct one-shot run toggles
pub async fn print_mode(
    prompt: &str,
    model: &str,
    provider_id: Option<&str>,
    profile: Profile,
    allow_writes: bool,
    self_review: bool,
    resume: Option<localpilot_core::SessionId>,
) -> anyhow::Result<PrintOutcome> {
    let cwd = std::env::current_dir()?;
    let mut runtime = build_runtime(&cwd, model, provider_id, profile, allow_writes).await?;
    if let Some(session) = resume {
        // Resume rebuilds the conversation from the durable event log; the
        // profile and trust just configured stay in force.
        runtime.load_session(session)?;
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
    let config = localpilot_config::load(&ConfigPaths::standard(cwd), &CliOverrides::default())?;
    let registry = ProviderRegistry::from_config(&config)?;
    let provider = match provider_id {
        Some(id) => registry.get(id),
        None => registry.default_provider(),
    }
    .cloned()
    .ok_or_else(|| anyhow::anyhow!("no provider is configured"))?;

    let context_token_limit = localpilot_harness::effective_context_limit(
        provider.declaration().max_context_tokens,
        config.harness.context_token_limit,
    );
    let mut registry = crate::mcp::McpTools::load(&config).await.registry();
    let broker = crate::mcp::install_broker(&config.tools, &mut registry);
    // Headless run (print/eval): apply the built-in safety rails so a project
    // with no `[harness]` budget/timeout still self-bounds (ADR-0055). Explicit
    // config values win inside `resolved_rails`.
    let rails = config.harness.resolved_rails(false);
    let mut runtime = SessionRuntime::new(
        provider,
        registry,
        PermissionEngine::new(profile, Vec::new()),
        Box::new(ScriptedApprover::new(Vec::new())),
        Store::open(cwd),
        workspace_with_read_roots(cwd, &config)?,
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
            turn_timeout: rails.turn_timeout_secs.map(std::time::Duration::from_secs),
            verify_before_done: config.harness.verify_before_done,
            verify_command: config.harness.verify_command.clone(),
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    runtime.set_broker(broker);
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
    if let Ok(id) = reference.parse::<localpilot_core::SessionId>() {
        return Ok(id);
    }
    let cwd = std::env::current_dir()?;
    let entry = Store::open(&cwd)
        .find_session_by_name(reference)?
        .ok_or_else(|| {
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
        let name = entry
            .name
            .as_deref()
            .map(|n| format!("  name: {n}"))
            .unwrap_or_default();
        writeln!(
            out,
            "{}  messages: {:<4} updated: {}{name}",
            entry.id, entry.message_count, entry.updated_unix
        )?;
    }
    Ok(())
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
        let mut consumer_gone = false;
        while let Ok(event) = rx.recv().await {
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
                        eprintln!("print: failed writing to stdout: {error}");
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
        eprintln!("handoff: {}", handoff.to_json_line());
    }

    if reason == StopReason::Degraded {
        eprintln!("warning: the model was marked degraded after repeated bad output");
    }
    Ok(PrintOutcome { consumer_gone })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        config.permissions.profile = localpilot_config::PermissionProfile::Unrestricted;
        assert_eq!(resolve_profile_from_config(&config), Profile::Unrestricted);
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
