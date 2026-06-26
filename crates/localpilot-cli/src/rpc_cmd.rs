//! `localpilot rpc` — drive the session runtime over stdin/stdout.
//!
//! Newline-delimited JSON: typed commands in, streamed session events out.
//! Permission asks are surfaced as events and answered by `permission_reply`
//! commands; an unanswered ask is denied, exactly like non-interactive mode.

use localpilot_config::{CliOverrides, ConfigPaths};
use localpilot_harness::{SessionConfig, SessionRuntime};
use localpilot_llm::ProviderRegistry;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_rpc::{serve, serve_acp, RpcApprover, ServeContext};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, Workspace};
use localpilot_store::Store;

/// Which stdio protocol to serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireProtocol {
    /// The native newline-delimited JSON protocol.
    Native,
    /// The Agent Client Protocol (JSON-RPC 2.0) for editors.
    Acp,
}

/// Serve one client on stdin/stdout until shutdown or end of input.
///
/// # Errors
/// Returns an error if configuration, the provider, or the workspace cannot
/// be set up, or the transport fails.
pub async fn run(
    model: Option<&str>,
    provider_id: Option<&str>,
    profile: Profile,
    protocol: WireProtocol,
) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let config = localpilot_config::load(&ConfigPaths::standard(&cwd), &CliOverrides::default())?;
    let model = model
        .map(str::to_string)
        .or_else(|| config.resolve_model(provider_id))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no model: pass --model, or set a default in .localpilot.toml \
                 ([providers.<id>] model = \"...\")"
            )
        })?;
    let registry = ProviderRegistry::from_config(&config)?;
    let provider = match provider_id {
        Some(id) => registry.get(id),
        None => registry.default_provider(),
    }
    .cloned()
    .ok_or_else(|| anyhow::anyhow!("no provider is configured"))?;

    let (approver, ask_rx, asks) = RpcApprover::new();
    let context_token_limit = localpilot_harness::effective_context_limit(
        provider.declaration().max_context_tokens,
        config.harness.context_token_limit,
    );
    let mut registry = crate::mcp::McpTools::load(&config).await.registry();
    let broker = crate::mcp::install_broker(&config.tools, &mut registry);
    // The serve loop is driven by a client (interactive): apply the built-in
    // safety rails so an unconfigured project still bounds a runaway, with the
    // interactive profile (higher ceiling, no default wall-clock). Explicit
    // `[harness]` values win inside `resolved_rails`.
    let rails = config.harness.resolved_rails(true);
    let mut runtime = SessionRuntime::new(
        provider,
        registry,
        PermissionEngine::new(profile, Vec::new()),
        Box::new(approver),
        Store::open(&cwd),
        Workspace::new(&cwd)?,
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            model: model.clone(),
            // The wire client answers asks; the engine itself treats the
            // session as interactive so ask-class effects reach the client
            // instead of being denied outright.
            interactivity: Interactivity::Interactive,
            trusted: profile == Profile::Bypass,
            context_token_limit,
            compaction_mode: compaction_mode(config.compaction.mode),
            summarizer_tuning: localpilot_harness::SummarizerTuning::from_config(
                &config.compaction,
            ),
            tool_call_budget: rails.tool_call_budget,
            tool_call_budget_max: rails.tool_call_budget_max,
            rules: config.harness.rules.clone(),
            enforce_claim_gate: config.harness.claim_gate.is_enabled(),
            tool_marker_enabled: config.tools.marker,
            enforce_readable_errors: config.tools.readable_errors,
            repair_mode: config.tools.repair,
            turn_timeout: rails.turn_timeout_secs.map(std::time::Duration::from_secs),
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    runtime.set_broker(broker);
    localpilot_harness::register_project_analysis_context(
        &cwd,
        config.context.project_analysis,
        config.docs.lookup_policy,
        &mut runtime,
    );

    // The Native serve path moves `cwd` into the serve context; keep a copy so
    // the session can be closed out into LocalMind when the client disconnects.
    let project_root = cwd.clone();
    match protocol {
        WireProtocol::Native => {
            let context = ServeContext {
                model,
                profile: profile_label(profile).to_string(),
                root: Some(cwd),
            };
            serve(
                &mut runtime,
                ask_rx,
                asks,
                tokio::io::stdin(),
                tokio::io::stdout(),
                &context,
            )
            .await?;
        }
        WireProtocol::Acp => {
            serve_acp(
                &mut runtime,
                ask_rx,
                asks,
                tokio::io::stdin(),
                tokio::io::stdout(),
            )
            .await?;
        }
    }
    // Learn from the served session on disconnect (best-effort; skips an empty
    // session), so editor/ACP sessions feed LocalMind like the REPL does.
    crate::context_inject::close_out(&project_root, runtime.session_id());
    Ok(())
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

fn profile_label(profile: Profile) -> &'static str {
    match profile {
        Profile::Default => "default",
        Profile::Relaxed => "relaxed",
        Profile::Bypass => "bypass",
    }
}
