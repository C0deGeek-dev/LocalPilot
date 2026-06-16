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
/// wins, and is never the default.
#[must_use]
pub fn resolve_profile(permission: Option<&str>, bypass: bool) -> Profile {
    if bypass {
        return Profile::Bypass;
    }
    match permission {
        Some("relaxed") => Profile::Relaxed,
        Some("bypass") => Profile::Bypass,
        _ => Profile::Default,
    }
}

/// Run print mode for one prompt.
///
/// # Errors
/// Returns an error if configuration, the provider registry, or the workspace
/// cannot be set up.
pub async fn print_mode(
    prompt: &str,
    model: &str,
    provider_id: Option<&str>,
    profile: Profile,
    allow_writes: bool,
    resume: Option<localpilot_core::SessionId>,
) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let config = localpilot_config::load(&ConfigPaths::standard(&cwd), &CliOverrides::default())?;
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
    let mut runtime = SessionRuntime::new(
        provider,
        crate::mcp::McpTools::load(&config).await.registry(),
        PermissionEngine::new(profile, Vec::new()),
        Box::new(ScriptedApprover::new(Vec::new())),
        Store::open(&cwd),
        Workspace::new(&cwd)?,
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            model: model.to_string(),
            interactivity: Interactivity::NonInteractive,
            trusted: allow_writes,
            context_token_limit,
            compaction_mode: compaction_mode(config.compaction.mode),
            summarizer_tuning: localpilot_harness::SummarizerTuning::from_config(
                &config.compaction,
            ),
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    localpilot_localmind::register_context_hook(&cwd, &mut runtime);
    if let Some(session) = resume {
        // Resume rebuilds the conversation from the durable event log; the
        // profile and trust just configured stay in force.
        runtime.load_session(session)?;
    }

    run_and_print(runtime, prompt).await
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

/// Resolve `--continue` / `--resume <id>` into a session id.
///
/// # Errors
/// Returns an error for an unparsable id, or `--continue` with no sessions.
pub fn resolve_resume(
    continue_latest: bool,
    resume: Option<&str>,
) -> anyhow::Result<Option<localpilot_core::SessionId>> {
    if let Some(id) = resume {
        return Ok(Some(id.parse()?));
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
        writeln!(
            out,
            "{}  messages: {:<4} updated: {}",
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

async fn run_and_print(mut runtime: SessionRuntime, prompt: &str) -> anyhow::Result<()> {
    let (events, mut rx) = broadcast::channel(1024);
    let cancel = CancellationToken::new();

    let printer = tokio::spawn(async move {
        let mut out = std::io::stdout();
        while let Ok(event) = rx.recv().await {
            match event {
                RuntimeEvent::Text(text) => {
                    let _ = write!(out, "{text}");
                    let _ = out.flush();
                }
                RuntimeEvent::Stopped(_) => break,
                _ => {}
            }
        }
    });

    let reason = runtime.run_turn(prompt, &events, &cancel).await;
    drop(events);
    let _ = printer.await;
    println!();

    if reason == StopReason::Degraded {
        eprintln!("warning: the model was marked degraded after repeated bad output");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_map_to_profiles() {
        assert_eq!(resolve_profile(None, false), Profile::Default);
        assert_eq!(resolve_profile(Some("relaxed"), false), Profile::Relaxed);
        assert_eq!(resolve_profile(Some("default"), false), Profile::Default);
        // --bypass always wins and is explicit.
        assert_eq!(resolve_profile(None, true), Profile::Bypass);
        assert_eq!(resolve_profile(Some("relaxed"), true), Profile::Bypass);
        assert_eq!(resolve_profile(Some("bypass"), false), Profile::Bypass);
    }
}
