//! `localpilot print` — a non-interactive, single-prompt agent run.
//!
//! Print mode runs the shared session loop once, streams the answer to stdout,
//! and makes no workspace mutations by default: it runs non-interactively, so the
//! permission engine denies write/destructive effects unless writes are
//! explicitly enabled.

use std::io::Write;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use localpilot_config::{CliOverrides, ConfigPaths};
use localpilot_harness::{RuntimeEvent, SessionConfig, SessionRuntime, StopReason};
use localpilot_llm::ProviderRegistry;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;

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
            context_token_limit: config.harness.context_token_limit,
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    crate::context_inject::seed(&cwd, &mut runtime, prompt);

    run_and_print(runtime, prompt).await
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
