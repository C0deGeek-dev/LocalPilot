//! Shared host operations for proposal execution and evidence inspection.

use std::io::Write;
use std::path::Path;

use anyhow::{anyhow, Context};
use localpilot_config::{CliOverrides, ConfigPaths};
use localpilot_harness::CancelSignal;
use localpilot_patchgen::validation::{
    load_validation_evidence, ContentDigest, EvaluationToken, ValidationState,
};
use localpilot_patchgen::ProposedPatch;
use localpilot_sandbox::PermissionEngine;

/// Recovery is a separate explicit metadata operation, never resumed consent.
#[derive(Debug, clap::Args)]
pub(crate) struct RecoveryArgs {
    /// Unfinished intent UUID. Omit to list pending public audit events.
    #[arg(long)]
    pub(crate) nonce: Option<uuid::Uuid>,
    #[arg(long)]
    pub(crate) reviewer: Option<String>,
    /// Acknowledge that remaining processes, host effects and remote publication
    /// have been inspected. Never retries the interrupted action.
    #[arg(long)]
    pub(crate) acknowledge_unknown_effects: bool,
    #[arg(long)]
    pub(crate) review: Option<String>,
    /// Human inspection/disposition note, retained in the public audit event.
    #[arg(long)]
    pub(crate) note: Option<String>,
}

pub(crate) fn recovery(
    root: &Path,
    args: &RecoveryArgs,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let Some(nonce) = args.nonce else {
        if args.acknowledge_unknown_effects {
            return Err(anyhow!(
                "recovery acknowledgement requires an exact --nonce and displayed --review"
            ));
        }
        let events = localpilot_selfimprove::pending_authority(root)?;
        if events.is_empty() {
            writeln!(out, "No unfinished authority.")?;
        }
        for event in events {
            writeln!(
                out,
                "{}",
                localpilot_config::redact::redact(&serde_json::to_string_pretty(&event)?)
            )?;
            writeln!(out, "Inspect with `selfimprove recovery --nonce {}`. No effect replayed or authority restored.", event.reservation.nonce)?;
        }
        return Ok(());
    };
    let review = localpilot_selfimprove::recovery_review(root, nonce)?;
    write!(out, "{}", review.render()?)?;
    out.flush()?;
    if !args.acknowledge_unknown_effects {
        writeln!(out, "No recovery acknowledged. After inspection, use --acknowledge-unknown-effects --reviewer <you> --note <inspection> --review {}.", review.digest()?.as_str())?;
        return Ok(());
    }
    confirm_review(args.review.as_deref(), &review.digest()?)?;
    localpilot_selfimprove::acknowledge_recovery(
        root,
        &review,
        args.reviewer
            .as_deref()
            .ok_or_else(|| anyhow!("recovery requires --reviewer"))?,
        args.note
            .as_deref()
            .ok_or_else(|| anyhow!("recovery requires --note describing the inspection"))?,
    )?;
    writeln!(out, "Acknowledged intent {nonce}; no checks, source promotion or publication replayed. Validation is not certified by recovery.")?;
    Ok(())
}

/// Compare a deliberate host confirmation to the previously displayed identity.
/// A missing or changed identity must fail before minting any consent.
pub(crate) fn confirm_review(expected: Option<&str>, actual: &ContentDigest) -> anyhow::Result<()> {
    if expected != Some(actual.as_str()) {
        return Err(anyhow!(
            "missing or changed review identity; inspect the current preview and pass its exact --review value"
        ));
    }
    Ok(())
}

/// Static preview without execution authority. Shared by both command surfaces.
pub(crate) fn preflight(root: &Path, id: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    let report = localpilot_selfimprove::proposal_preflight(root, id)?;
    write!(out, "{}", report.render())?;
    Ok(())
}

/// Execute only after explicit, separate consent naming the displayed preview.
/// This path loads no model/provider and never promotes or publishes.
pub(crate) async fn validate(
    root: &Path,
    id: &str,
    reviewer: Option<&str>,
    approve_execution: bool,
    expected_review: Option<&str>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let report = localpilot_selfimprove::proposal_preflight(root, id)
        .context("preparing proposal execution review")?;
    write!(out, "{}", report.render())?;
    out.flush()?;
    if !approve_execution {
        writeln!(out, "No execution authorized. To run this exact plan, use --approve-execution --reviewer <you> --review {}. Ctrl+C cancels an active run; it never retries or promotes.", report.digest()?.as_str())?;
        return Ok(());
    }
    confirm_review(expected_review, &report.digest()?)?;
    let reviewer = reviewer
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| {
            anyhow!("--reviewer must name the human authorizing proposed code execution")
        })?;
    let trusted = crate::trust::is_trusted(root);
    if !trusted {
        return Err(anyhow!("proposal execution requires a trusted project; inspect it and use `localpilot trust add` first"));
    }
    let config = localpilot_config::load(&ConfigPaths::standard(root), &CliOverrides::default())?;
    let profile = crate::session_cmd::resolve_profile_from_config(&config);
    let engine = PermissionEngine::new(profile, Vec::new())
        .with_allowed_commands(crate::session_cmd::allowed_commands(&config));
    writeln!(
        out,
        "Execution permission profile: {profile:?}; existing Deny decisions remain enforced."
    )?;
    out.flush()?;
    let cancel = CancelSignal::new();
    let interrupt = Interrupt(tokio::spawn({
        let cancel = cancel.clone();
        async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.cancel();
            }
        }
    }));
    let result = execute(root, id, &report, reviewer, &engine, trusted, cancel, out).await;
    drop(interrupt);
    result
}

/// Execute the host-confirmed review with the host's current permission/trust
/// snapshot and cooperative cancellation. It never broadens that policy.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute(
    root: &Path,
    id: &str,
    report: &localpilot_selfimprove::PreflightReport,
    reviewer: &str,
    engine: &PermissionEngine,
    trusted: bool,
    cancel: CancelSignal,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    if !trusted {
        return Err(anyhow!("proposal execution requires a trusted project"));
    }
    let token = EvaluationToken::approve(
        &report.proposal.identity,
        &report.plan,
        reviewer,
        "execute only this explicitly confirmed proposal preview",
    )?;
    let receipt = localpilot_selfimprove::validate_proposal(
        root, id, report, token, engine, trusted, cancel, out,
    )
    .await
    .context("validating the exact proposal; inspect authority recovery if interrupted")?;
    writeln!(
        out,
        "Validation {:?}; run {}. No promotion performed.",
        receipt.state, receipt.run_id
    )?;
    if receipt.state != ValidationState::Passed {
        return Err(anyhow!("proposal validation did not pass; inspect `self-review evidence --id {id}` before a separately authorized retry"));
    }
    Ok(())
}

// Abort the signal waiter on every exit, including cancellation of this future.
struct Interrupt(tokio::task::JoinHandle<()>);

impl Drop for Interrupt {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Inspect bounded stored data. Historical or current JSON is never authority.
pub(crate) fn evidence(
    root: &Path,
    id: &str,
    run: Option<uuid::Uuid>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let receipt = match run {
        Some(run) => load_validation_evidence(root, run)?,
        None => match ProposedPatch::reopen(root, id)?
            .provenance()
            .validation
            .as_ref()
        {
            Some(receipt) => receipt.clone(),
            None => {
                writeln!(out, "Validation: NotRun. Inspect `self-review preflight --id {id}`; viewing it authorizes nothing.")?;
                return Ok(());
            }
        },
    };
    if receipt.approved_proposal.proposal_id != id {
        return Err(anyhow!(
            "the requested receipt belongs to a different proposal"
        ));
    }
    writeln!(
        out,
        "Recorded evidence only; historical data does not authorize execution or promotion."
    )?;
    writeln!(
        out,
        "{}",
        localpilot_config::redact::redact(&serde_json::to_string_pretty(&receipt)?)
    )?;
    match localpilot_selfimprove::promotion_review(root, id) {
        Ok(review) if receipt == review.evidence => {
            writeln!(
                out,
                "Current passing evidence verified locally. Promotion review identity: {}",
                review.digest()?.as_str()
            )?;
        }
        Ok(review) if receipt.run_id == review.evidence.run_id => writeln!(out,
            "Displayed receipt differs from the current locally verified evidence; no promotion review issued.")?,
        Ok(_) => writeln!(
            out,
            "Historical receipt; current promotion evidence is a different run."
        )?,
        Err(error) => writeln!(out, "Current promotion unavailable: {error}")?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmation_requires_the_exact_displayed_identity() {
        let current = ContentDigest::for_bytes(b"current");
        assert!(confirm_review(None, &current).is_err());
        assert!(confirm_review(Some("old"), &current).is_err());
        assert!(confirm_review(Some(current.as_str()), &current).is_ok());
    }

    #[cfg(feature = "tui")]
    #[test]
    #[allow(clippy::unwrap_used)]
    fn selfimprove_validation_history_rejects_changed_data_even_when_the_run_id_matches() {
        use localpilot_patchgen::validation::{
            RepositoryAuthority, ValidationCheckEvidence, ValidationCommandEnd, ValidationEvidence,
            VALIDATION_SCHEMA,
        };
        let mut fixture = crate::proposal_fixture::fixture("#[test] fn controlled() {}\n");
        let report =
            localpilot_selfimprove::proposal_preflight(fixture.root.path(), fixture.patch.id())
                .unwrap();
        let mut authority = RepositoryAuthority::open(fixture.root.path()).unwrap();
        let mut tx = authority.try_transaction().unwrap();
        let token = EvaluationToken::approve(
            &report.proposal.identity,
            &report.plan,
            "fixture observer",
            "scripted trusted observations for display binding",
        )
        .unwrap();
        let run = tx
            .reserve_evaluation(token, &report.proposal.identity, &report.plan)
            .unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Scripted trusted observations isolate the display binding. Other host
        // tests run actual Cargo; this fixture makes no execution-quality claim.
        let receipt = ValidationEvidence {
            schema: VALIDATION_SCHEMA.into(),
            run_id: run,
            proposal: report.proposal.identity.digest(),
            approved_proposal: report.proposal.identity.clone(),
            plan: report.plan.digest().unwrap(),
            approved_plan: report.plan.clone(),
            state: ValidationState::Passed,
            started_at: now,
            finished_at: now,
            checks: report
                .plan
                .checks
                .iter()
                .map(|command| ValidationCheckEvidence {
                    command: command.clone(),
                    end: ValidationCommandEnd::Exited { code: 0 },
                    elapsed_ms: 0,
                    stdout: "trusted recorded observation".into(),
                    stderr: String::new(),
                    truncated: false,
                    pipes_closed: true,
                    cleanup_confirmed: true,
                })
                .collect(),
            artifacts: Vec::new(),
        };
        fixture
            .patch
            .record_validation(&tx, receipt.clone())
            .unwrap();
        tx.finish_evaluation(&receipt).unwrap();
        drop(tx);
        let mut altered = receipt;
        altered.checks[0].stdout = "changed historical output with the same UUID".into();
        std::fs::write(
            fixture
                .root
                .path()
                .join(format!(".localpilot/validation/receipts/{run}.json")),
            serde_json::to_vec(&altered).unwrap(),
        )
        .unwrap();
        let mut output = Vec::new();
        evidence(
            fixture.root.path(),
            fixture.patch.id(),
            Some(run),
            &mut output,
        )
        .unwrap();
        let shown = String::from_utf8(output).unwrap();
        assert!(
            !shown.contains("Current passing evidence verified locally"),
            "changed historical data must not borrow the current receipt's verified status"
        );
        assert!(!shown.contains("Promotion review identity:"));
    }
}
