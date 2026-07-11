//! `localpilot rpc` — drive the session runtime over stdin/stdout.
//!
//! Newline-delimited JSON: typed commands in, streamed session events out.
//! Permission asks are surfaced as events and answered by `permission_reply`
//! commands; an unanswered ask is denied, exactly like non-interactive mode.

use localpilot_config::{CliOverrides, ConfigPaths};
use localpilot_harness::{SessionConfig, SessionRuntime};
use localpilot_llm::ProviderRegistry;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_rpc::{serve, serve_acp, serve_mcp, McpServeOptions, RpcApprover, ServeContext};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile};
use localpilot_store::Store;

/// Which stdio protocol to serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireProtocol {
    /// The native newline-delimited JSON protocol.
    Native,
    /// The Agent Client Protocol (JSON-RPC 2.0) for editors.
    Acp,
    /// The Model Context Protocol (JSON-RPC 2.0) for agent hosts. When
    /// `approvals` is false the permission-reply tool is withheld — the
    /// client can watch and steer but every ask denies.
    Mcp {
        /// Expose the permission-reply tool.
        approvals: bool,
    },
}

/// Serve one client on stdin/stdout until shutdown or end of input.
///
/// # Errors
/// Returns an error if configuration, the provider, or the workspace cannot
/// be set up, a resumed session's event log cannot be read, or the transport
/// fails.
pub async fn run(
    model: Option<&str>,
    provider_id: Option<&str>,
    profile: Profile,
    protocol: WireProtocol,
    resume: Option<localpilot_core::SessionId>,
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
        crate::session_cmd::workspace_with_read_roots(&cwd, &config)?,
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            model: model.clone(),
            // The wire client answers asks; the engine itself treats the
            // session as interactive so ask-class effects reach the client
            // instead of being denied outright.
            interactivity: Interactivity::Interactive,
            trusted: matches!(profile, Profile::Bypass | Profile::Unrestricted),
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
        &cwd,
        config.context.project_analysis,
        config.docs.lookup_policy,
        &mut runtime,
    );
    localpilot_harness::register_project_instructions_context(
        &cwd,
        config.context.inject_instructions,
        config.context.instruction_char_budget,
        &mut runtime,
    );

    // Resume before serving so the handshake reports the resumed session's id;
    // the current profile and trust stay in force, exactly as the REPL's resume.
    if let Some(session) = resume {
        runtime.load_session(session)?;
    }

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
        WireProtocol::Mcp { approvals } => {
            let options = McpServeOptions {
                model,
                profile: profile_label(profile).to_string(),
                root: Some(cwd),
                approvals,
            };
            let report = serve_mcp(
                &mut runtime,
                ask_rx,
                asks,
                tokio::io::stdin(),
                tokio::io::stdout(),
                &options,
            )
            .await?;
            offer_intervention_lessons(&project_root, &report);
        }
    }
    // Learn from the served session on disconnect (best-effort; skips an empty
    // session), so editor/ACP sessions feed LocalMind like the REPL does.
    crate::context_inject::close_out(&project_root, runtime.session_id());
    Ok(())
}

/// Cap on driver-intervention lesson candidates offered per served session,
/// so one noisy session cannot flood the review queue.
const MAX_INTERVENTION_LESSONS: usize = 8;

/// Offer a served session's driver corrections to the review-gated lesson
/// queue with honest provenance (the queue entry names the driving client).
/// Advisory best-effort, like the completion-retrospective wire: a failed
/// enqueue never breaks a finished session, and a candidate reaches memory
/// only after human review. Approvals are event-log-only — routine consent is
/// not a lesson; corrections (steers, cancels, denials) are.
fn offer_intervention_lessons(
    project_root: &std::path::Path,
    report: &localpilot_rpc::McpServeReport,
) {
    let mut offered = 0usize;
    for record in report
        .interventions
        .iter()
        .filter(|record| record.action != "allow")
        .take(MAX_INTERVENTION_LESSONS)
    {
        // Lead with the distinct correction and keep the framing minimal:
        // the review queue folds lexical near-duplicates, so boilerplate-heavy
        // phrasing would merge different corrections into one candidate. The
        // full context (activity, client) stays in the event log and the
        // candidate's evidence.
        let doing = record
            .activity
            .as_deref()
            .map(|activity| format!(" during {activity}"))
            .unwrap_or_default();
        let text = match record.action.as_str() {
            "steer" => format!("Driver steer: {}", record.detail),
            "cancel" => format!("Driver cancelled the turn{doing}"),
            _ => format!("Driver denied {}", record.detail),
        };
        if let Ok(Some(_)) = localpilot_localmind::write_retrospective_lesson(
            project_root,
            &localpilot_localmind::RetrospectiveLesson::driver_intervention(text, &report.client),
        ) {
            offered += 1;
        }
    }
    if offered > 0 {
        eprintln!("learning: offered {offered} driver-intervention candidate(s) to review");
    }
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
        Profile::Unrestricted => "unrestricted",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use localpilot_rpc::{DriverInterventionRecord, McpServeReport};

    fn record(action: &str, detail: &str) -> DriverInterventionRecord {
        DriverInterventionRecord {
            action: action.to_string(),
            detail: detail.to_string(),
            activity: Some("running run_shell".to_string()),
        }
    }

    #[test]
    fn corrections_become_review_candidates_but_approvals_do_not() {
        // Bug it prevents: routine approvals flooding the review queue, or
        // corrections silently never reaching it.
        let dir = tempfile::tempdir().unwrap();
        let report = McpServeReport {
            client: "test-host 1.0".to_string(),
            interventions: vec![
                record("steer", "run the failing test first"),
                record("allow", "run_shell: cargo test"),
                record("deny", "run_shell: rm -rf build"),
                record("cancel", "turn cancelled"),
            ],
        };

        offer_intervention_lessons(dir.path(), &report);

        let items = localpilot_localmind::review_list(dir.path()).unwrap();
        assert_eq!(items.len(), 3, "steer + deny + cancel, never allow");
        assert!(items
            .iter()
            .all(|item| item.session_id == "driver-intervention"));
    }

    #[test]
    fn a_noisy_session_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let interventions = (0..20)
            .map(|n| record("steer", &format!("prefer tactic{n:02} in area{n:02}")))
            .collect();
        let report = McpServeReport {
            client: "test-host 1.0".to_string(),
            interventions,
        };

        offer_intervention_lessons(dir.path(), &report);

        let items = localpilot_localmind::review_list(dir.path()).unwrap();
        assert_eq!(items.len(), MAX_INTERVENTION_LESSONS);
    }
}
