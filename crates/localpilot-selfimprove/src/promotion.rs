//! Common locked promotion service for direct and orchestrated hosts.

use crate::orchestrator::{read_loop_state, write_loop_state};
use crate::preflight::{preflight_in_transaction, PreflightReport};
use crate::{LoopError, Stage};
use localpilot_patchgen::validation::{
    ContentDigest, PatchApprovalToken, RepositoryAuthority, RepositoryTransaction, ValidationError,
    ValidationEvidence, ValidationState,
};
use localpilot_patchgen::{PromoteOutcome, ProposedPatch};
use std::path::Path;

/// Inspectable review data, never a promotion capability or replayable consent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotionReview {
    /// Fresh static source/toolchain/command plan.
    pub preflight: PreflightReport,
    /// Exact locally completed passing observations.
    pub evidence: ValidationEvidence,
}

impl PromotionReview {
    /// Combined immutable preview identity, suitable for a host confirmation.
    /// Both components are fixed-width canonical SHA256 hex digests.
    ///
    /// # Errors
    /// Unsupported plan or observation data.
    pub fn digest(&self) -> Result<ContentDigest, ValidationError> {
        Ok(ContentDigest::for_bytes(
            format!(
                "localpilot-promotion-review-v1\0{}\0{}",
                self.preflight.digest()?.as_str(),
                self.evidence.digest()?.as_str()
            )
            .as_bytes(),
        ))
    }

    /// Show the source/plan and actual bounded observations before separate
    /// promotion confirmation. This rendering performs no execution or promotion.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = self.preflight.render();
        out.push_str(&format!(
            "\nRecorded validation: {:?}; run {}; completed at Unix {}\n",
            self.evidence.state, self.evidence.run_id, self.evidence.finished_at
        ));
        for check in &self.evidence.checks {
            out.push_str(&format!("{}: {:?}, {}ms; cleanup confirmed={}, pipes closed={}, output truncated={}\nstdout:\n{}\nstderr:\n{}\n",
                check.command.name, check.end, check.elapsed_ms, check.cleanup_confirmed,
                check.pipes_closed, check.truncated, check.stdout, check.stderr));
        }
        out.push_str("Promotion approval applies only to this already-validated proposal. It does not run or retry checks.\n");
        localpilot_config::redact::redact(&out)
    }
}

fn review_in_transaction(
    root: &Path,
    patch: &ProposedPatch,
    transaction: &RepositoryTransaction<'_>,
) -> Result<PromotionReview, LoopError> {
    transaction.require_idle()?;
    let evidence = patch
        .provenance()
        .validation
        .as_ref()
        .ok_or(ValidationError::NotValidated(ValidationState::NotRun))?;
    if evidence.state != ValidationState::Passed {
        return Err(ValidationError::NotValidated(evidence.resumed_state()).into());
    }
    let preflight = preflight_in_transaction(root, patch, transaction)?;
    transaction.verify_validation(&preflight.proposal.identity, &preflight.plan, evidence)?;
    Ok(PromotionReview {
        preflight,
        evidence: evidence.clone(),
    })
}

/// Resolve a current, locally authenticated promotion review without minting
/// consent. Release the lock before asking a human; promotion reacquires it.
///
/// # Errors
/// Unvalidated, failed, stale, expired or interrupted proposal/configuration.
pub fn promotion_review(root: &Path, id: &str) -> Result<PromotionReview, LoopError> {
    let mut authority = RepositoryAuthority::open(root)?;
    let transaction = authority.try_transaction()?;
    let patch = ProposedPatch::reopen(root, id)?;
    review_in_transaction(root, &patch, &transaction)
}

/// Promote an explicitly approved exact review, then atomically update a matching
/// active loop before completing the journal. Direct and orchestrated hosts use
/// this same transaction; no commands are executed or retried here.
///
/// # Errors
/// Changed review, wrong/expired/reused consent, invalid state, unfinished
/// authority, refused fast-forward or failed durable state/journal write.
pub fn promote_proposal(
    root: &Path,
    id: &str,
    review: &PromotionReview,
    token: PatchApprovalToken,
) -> Result<PromoteOutcome, LoopError> {
    promote_for_loop(root, id, review, token, false)
}

pub(crate) fn promote_for_loop(
    root: &Path,
    id: &str,
    review: &PromotionReview,
    token: PatchApprovalToken,
    require_loop: bool,
) -> Result<PromoteOutcome, LoopError> {
    let mut authority = RepositoryAuthority::open(root)?;
    let transaction = authority.try_transaction()?;
    let patch = ProposedPatch::reopen(root, id)?;
    transaction.require_idle()?;
    let mut state = read_loop_state(root)?;
    let matching = state
        .as_ref()
        .is_some_and(|state| state.proposal_id.as_deref() == Some(id));
    if require_loop && !matching {
        return Err(if state.is_none() {
            LoopError::NoActiveLoop {
                attempted: "approve",
            }
        } else {
            ValidationError::Stale.into()
        });
    }
    if let Some(state) = state.as_ref().filter(|_| matching) {
        if state.stage != Stage::Proposed {
            return Err(LoopError::OutOfOrder {
                attempted: "approve",
                actual: state.stage,
            });
        }
    }
    let current = review_in_transaction(root, &patch, &transaction)?;
    if current != *review || current.digest()? != review.digest()? {
        return Err(ValidationError::Stale.into());
    }
    let proof = transaction.prepare_promotion(&patch, &current.preflight.plan, token)?;
    let pending = patch.promote(proof)?;
    if let Some(state) = state.as_mut().filter(|_| matching) {
        state.stage = Stage::Approved;
        state.reviewer = Some(pending.outcome().reviewer.clone());
        write_loop_state(root, state)?;
    }
    Ok(pending.finish()?)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::orchestrator::{BuildRecord, LoopState, Orchestrator, SelfDevStage, StageError};
    use localpilot_harness::CancelSignal;
    use localpilot_sandbox::Profile;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    struct RecordedSelfDev(Arc<Mutex<Vec<String>>>);
    impl SelfDevStage for RecordedSelfDev {
        fn build_and_promote(
            &mut self,
            root: &Path,
            _: &mut dyn Write,
        ) -> Result<BuildRecord, StageError> {
            self.0
                .lock()
                .unwrap()
                .push(format!("build:{}", root.display()));
            Ok(BuildRecord {
                label: "fixture-build".into(),
            })
        }
        fn reload_onto(
            &mut self,
            record: &BuildRecord,
            _: &mut dyn Write,
        ) -> Result<(), StageError> {
            self.0
                .lock()
                .unwrap()
                .push(format!("reload:{}", record.label));
            Ok(())
        }
    }

    fn consent(review: &PromotionReview) -> PatchApprovalToken {
        PatchApprovalToken::approve(
            &review.preflight.proposal.identity,
            &review.preflight.plan,
            &review.evidence,
            "fixture owner",
            "promote the exact reviewed fixture",
        )
        .unwrap()
    }

    #[tokio::test]
    async fn common_direct_promotion_checkpoints_the_matching_loop_and_preserves_post_promotion_stages(
    ) {
        common_promotion_flow(false).await;
    }

    #[tokio::test]
    async fn orchestrated_promotion_requires_the_same_evidence_and_preserves_post_promotion_stages()
    {
        common_promotion_flow(true).await;
    }

    async fn common_promotion_flow(orchestrated: bool) {
        let (root, patch, base) = crate::execution::tests::fixture("#[test] fn works() {}\n");
        let receipt = crate::execution::tests::run(
            root.path(),
            &patch,
            Profile::Default,
            CancelSignal::new(),
        )
        .await;
        assert_eq!(receipt.state, ValidationState::Passed);
        let review = promotion_review(root.path(), patch.id()).unwrap();
        assert_eq!(review.evidence, receipt);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let selfdev_root = tempfile::tempdir().unwrap();
        let mut loop_ = Orchestrator::open(
            root.path(),
            selfdev_root.path(),
            RecordedSelfDev(calls.clone()),
        );
        assert!(matches!(
            loop_.approve(&review, consent(&review)),
            Err(LoopError::NoActiveLoop { .. })
        ));
        write_loop_state(
            root.path(),
            &LoopState {
                stage: Stage::Proposed,
                proposal_id: Some(patch.id().into()),
                built_label: None,
                reviewer: None,
            },
        )
        .unwrap();
        let outcome = if orchestrated {
            loop_.approve(&review, consent(&review))
        } else {
            promote_proposal(root.path(), patch.id(), &review, consent(&review))
        }
        .unwrap();
        assert_eq!(outcome.reviewer, "fixture owner");
        let state = loop_.state().unwrap().unwrap();
        assert_eq!(state.stage, Stage::Approved);
        assert!(state.built_label.is_none());
        assert!(
            calls.lock().unwrap().is_empty(),
            "validation/promotion must not advance the self-dev stages"
        );
        let head = std::process::Command::new("git")
            .current_dir(root.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert_ne!(String::from_utf8(head.stdout).unwrap().trim(), base);
        // A second consent cannot replay the already advanced loop.
        assert!(matches!(
            promote_proposal(root.path(), patch.id(), &review, consent(&review)),
            Err(LoopError::OutOfOrder { .. })
        ));
        loop_.build(&mut Vec::new()).unwrap();
        assert_eq!(loop_.state().unwrap().unwrap().stage, Stage::Built);
        loop_.reload(&mut Vec::new()).unwrap();
        assert_eq!(loop_.state().unwrap().unwrap().stage, Stage::Reloaded);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                format!("build:{}", root.path().display()),
                "reload:fixture-build".into()
            ]
        );
        let mut authority = RepositoryAuthority::open(root.path()).unwrap();
        let transaction = authority.try_transaction().unwrap();
        assert_eq!(transaction.events().count(), 2);
        assert!(transaction.events().all(|event| event.completed));
    }

    #[test]
    fn an_unvalidated_persisted_proposal_stays_inspectable_and_cannot_be_reviewed_as_passed() {
        let (root, patch, _) = crate::execution::tests::fixture("#[test] fn works() {}\n");
        assert!(matches!(
            promotion_review(root.path(), patch.id()),
            Err(LoopError::Validation(ValidationError::NotValidated(
                ValidationState::NotRun
            )))
        ));
        assert!(ProposedPatch::reopen(root.path(), patch.id())
            .unwrap()
            .worktree_path()
            .exists());
        let mut authority = RepositoryAuthority::open(root.path()).unwrap();
        assert_eq!(authority.try_transaction().unwrap().events().count(), 0);
    }
}
