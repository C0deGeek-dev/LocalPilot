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
        Ok(review) if run.is_none_or(|run| run == review.evidence.run_id) => {
            writeln!(
                out,
                "Current passing evidence verified locally. Promotion review identity: {}",
                review.digest()?.as_str()
            )?;
        }
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
}
