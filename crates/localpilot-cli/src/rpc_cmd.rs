//! `localpilot rpc` — drive the session runtime over stdin/stdout.
//!
//! Newline-delimited JSON: typed commands in, streamed session events out.
//! Permission asks are surfaced as events and answered by `permission_reply`
//! commands; an unanswered ask is denied, exactly like non-interactive mode.

use localpilot_rpc::{serve, serve_acp, serve_mcp, McpServeOptions, ServeContext};
use localpilot_sandbox::Profile;

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
/// Returns an error if configuration, the provider, or the workspace cannot be
/// set up, a resumed session's event log cannot be read, or the transport
/// fails.
pub async fn run(
    model: Option<&str>,
    provider_id: Option<&str>,
    profile: Profile,
    protocol: WireProtocol,
    resume: Option<localpilot_core::SessionId>,
) -> anyhow::Result<()> {
    // Construct the session through the one shared recipe (also used by the
    // opt-in server factory), so the stdio and server paths never drift.
    let setup = crate::server_cmd::SessionSetup::resolve(model, provider_id, profile).await?;
    let model = setup.model().to_string();
    let cwd = setup.cwd().to_path_buf();
    let profile_label = crate::server_cmd::profile_label(profile).to_string();
    let built = setup.build()?;
    let mut runtime = built.runtime;
    let ask_rx = built.ask_rx;
    let asks = built.asks;

    // Resume before serving so the handshake reports the resumed session's id;
    // the current profile and trust stay in force, exactly as the REPL's resume.
    if let Some(session) = resume {
        let report = runtime.load_session(session)?;
        if report.skipped_lines > 0 {
            // stdout carries the protocol; the recovery note goes to stderr.
            eprintln!(
                "resume: skipped {} damaged event line(s); continuing with the intact log",
                report.skipped_lines
            );
        }
    }

    // The Native serve path moves `cwd` into the serve context; keep a copy so
    // the session can be closed out into LocalMind when the client disconnects.
    let project_root = cwd.clone();
    match protocol {
        WireProtocol::Native => {
            let context = ServeContext {
                model,
                profile: profile_label,
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
                profile: profile_label,
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
