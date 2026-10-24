//! Locked, memory-only proof and an explicit post-effect checkpoint.

use super::{
    PatchApprovalToken, ProposalIdentity, RepositoryTransaction, ValidationError,
    ValidationPlanIdentity, ValidationState,
};
use crate::{PromoteOutcome, ProposedPatch};
use std::fmt;

/// Ephemeral proof owned by a common-repository transaction, never reconstructed
/// from persisted receipt data. Its fields and constructor are private; only
/// locked current revalidation can produce it. Holding it retains the lock.
///
/// ```compile_fail
/// use localpilot_patchgen::validation::ValidatedProposal;
/// fn restore(json: &str) -> ValidatedProposal<'_> { serde_json::from_str(json).unwrap() }
/// ```
/// ```compile_fail
/// use localpilot_patchgen::{ProposedPatch, validation::PatchApprovalToken};
/// fn bypass(patch: &ProposedPatch, token: PatchApprovalToken) {
///     patch.promote(token);
/// }
/// ```
pub struct ValidatedProposal<'repo> {
    transaction: RepositoryTransaction<'repo>,
    proposal: ProposalIdentity,
    plan: ValidationPlanIdentity,
    token: PatchApprovalToken,
    caller_checkpoint: super::CallerCheckpoint,
}

impl ValidatedProposal<'_> {
    /// Declare the owning caller's logical state checkpoint while retaining this
    /// guard. The caller must derive both states under the same transaction.
    #[must_use]
    pub fn with_caller_checkpoint(mut self, checkpoint: super::CallerCheckpoint) -> Self {
        self.caller_checkpoint = checkpoint;
        self
    }
}

impl fmt::Debug for ValidatedProposal<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ValidatedProposal { authority: [redacted] }")
    }
}

impl<'repo> RepositoryTransaction<'repo> {
    /// Revalidate source, complete receipt, local execution record and private
    /// host context while retaining this exclusive transaction. A host must
    /// freshly resolve the trusted plan before calling this method. Promotion
    /// repeats these checks and consumes the grant immediately before Git.
    ///
    /// # Errors
    /// Not validated, stale/expired/malformed observations, interrupted authority,
    /// different repository or stale promotion consent.
    pub fn prepare_promotion(
        self,
        patch: &ProposedPatch,
        plan: &ValidationPlanIdentity,
        token: PatchApprovalToken,
    ) -> Result<ValidatedProposal<'repo>, ValidationError> {
        let receipt = patch
            .provenance()
            .validation
            .as_ref()
            .ok_or(ValidationError::NotValidated(ValidationState::NotRun))?;
        if receipt.state != ValidationState::Passed {
            return Err(ValidationError::NotValidated(receipt.resumed_state()));
        }
        let current = patch.inspect_validation(&self, plan)?;
        token.reservation(&current.identity, plan, receipt)?;
        self.verify_completed_evaluation(&current.identity, plan, receipt)?;
        Ok(ValidatedProposal {
            transaction: self,
            proposal: current.identity,
            plan: plan.clone(),
            token,
            caller_checkpoint: super::CallerCheckpoint::Unspecified,
        })
    }
}

/// Promotion happened, but associated caller state must be durably checkpointed
/// before [`Self::finish`]. Dropping this object leaves the nonce unfinished and
/// retains the exact old/new ref goal for explicit recovery, never automatic retry.
/// The common lock stays held throughout the caller's checkpoint.
pub struct PendingPromotion<'repo> {
    transaction: RepositoryTransaction<'repo>,
    nonce: uuid::Uuid,
    outcome: PromoteOutcome,
}

impl fmt::Debug for PendingPromotion<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PendingPromotion { authority: [redacted] }")
    }
}

impl PendingPromotion<'_> {
    /// Observed Git outcome, available while the repository lock is held.
    #[must_use]
    pub fn outcome(&self) -> &PromoteOutcome {
        &self.outcome
    }

    /// Call only after the host durably writes all associated state. A failed
    /// write must drop this object without completing its journal entry.
    ///
    /// # Errors
    /// Journal persistence failed or the authority store requires inspection.
    pub fn finish(mut self) -> Result<PromoteOutcome, ValidationError> {
        self.transaction.finish(
            self.nonce,
            "exact source promotion observed; caller checkpoint persisted",
        )?;
        Ok(self.outcome)
    }
}

impl ProposedPatch {
    /// Promote only the exact current proof, retaining the lock through the
    /// caller's durable checkpoint. All source-promotion callers require this
    /// capability; a patch approval token alone cannot perform the Git effect.
    ///
    /// # Errors
    /// Changed source/plan/receipt/context, expired or wrong grant, interrupted
    /// authority, dirty target or refused fast-forward. Ambiguous Git effects
    /// leave the reserved intent unfinished for inspection.
    pub fn promote<'repo>(
        &self,
        validated: ValidatedProposal<'repo>,
    ) -> Result<PendingPromotion<'repo>, ValidationError> {
        self.promote_after_reservation(validated, || {})
    }

    // Fault-injection seam is private. The production path supplies no callback
    // behavior and exposes no environment variable or flag to interrupt a run.
    fn promote_after_reservation<'repo>(
        &self,
        mut validated: ValidatedProposal<'repo>,
        before_effect: impl FnOnce(),
    ) -> Result<PendingPromotion<'repo>, ValidationError> {
        let current = self.inspect_validation(&validated.transaction, &validated.plan)?;
        if current.identity != validated.proposal {
            return Err(ValidationError::Stale);
        }
        let receipt = self
            .provenance()
            .validation
            .as_ref()
            .ok_or(ValidationError::NotValidated(ValidationState::NotRun))?;
        let reservation =
            validated
                .token
                .reservation(&current.identity, &validated.plan, receipt)?;
        let nonce = validated.transaction.reserve_promotion_with_checkpoint(
            validated.token,
            &current.identity,
            &validated.plan,
            receipt,
            validated.caller_checkpoint,
        )?;
        before_effect();
        // Pinned commit, never a branch that another actor could retarget.
        let outcome =
            self.fast_forward(&current.identity.proposed_commit, &reservation.reviewer)?;
        Ok(PendingPromotion {
            transaction: validated.transaction,
            nonce,
            outcome,
        })
    }
}

/// Scripted observations for Git-boundary unit fixtures only. The real runner
/// and trusted profile resolution are covered by coordinator integrations.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) fn promote_fixture(patch: &mut ProposedPatch) -> crate::PromoteOutcome {
    let mut authority = super::RepositoryAuthority::open(&patch.repo_root).unwrap();
    let mut transaction = authority.try_transaction().unwrap();
    let plan = tests::observed_plan(patch, &mut transaction);
    let token = tests::approval(patch, &transaction, &plan);
    let proof = transaction.prepare_promotion(patch, &plan, token).unwrap();
    patch.promote(proof).unwrap().finish().unwrap()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::validation::{self, EvaluationToken, RepositoryAuthority};
    use crate::{git, ChangeProvenance, PatchProposal, ProposedEdit};
    use std::fs;

    fn fixture() -> (tempfile::TempDir, ProposedPatch) {
        let repo = tempfile::tempdir().unwrap();
        git::git(repo.path(), &["init", "-b", "main"]).unwrap();
        git::git(repo.path(), &["config", "user.name", "fixture"]).unwrap();
        git::git(repo.path(), &["config", "user.email", "fixture@localhost"]).unwrap();
        git::git(repo.path(), &["config", "core.autocrlf", "false"]).unwrap();
        fs::write(repo.path().join(".gitignore"), ".localpilot/\n").unwrap();
        fs::write(
            repo.path().join("sample.rs"),
            "pub fn value() -> u32 { 1 }\n",
        )
        .unwrap();
        git::git(repo.path(), &["add", "."]).unwrap();
        git::git(
            repo.path(),
            &["commit", "-m", "controlled promotion fixture"],
        )
        .unwrap();
        let patch = crate::propose(
            repo.path(),
            "validated-fixture",
            &PatchProposal::new(
                "controlled source change",
                vec!["sample.rs".into()],
                vec![ProposedEdit::new(
                    "sample.rs",
                    "pub fn value() -> u32 { 2 }\n",
                )],
            ),
            ChangeProvenance::new("fixture", "scripted", "locked promotion proof"),
        )
        .unwrap();
        (repo, patch)
    }

    pub(super) fn observed_plan(
        patch: &mut ProposedPatch,
        transaction: &mut RepositoryTransaction<'_>,
    ) -> ValidationPlanIdentity {
        observed_plan_result(patch, transaction, false)
    }

    fn observed_plan_result(
        patch: &mut ProposedPatch,
        transaction: &mut RepositoryTransaction<'_>,
        failed: bool,
    ) -> ValidationPlanIdentity {
        // This crate's unit fixture supplies trusted process observations. Real
        // host execution and registered-plan resolution are integration-tested
        // by the self-improvement coordinator; no proposed module is imported.
        let (_, mut plan, mut receipt) =
            validation::tests::fixture(transaction.repository_identity());
        plan.worktree = fs::canonicalize(patch.worktree_path())
            .unwrap()
            .to_str()
            .unwrap()
            .into();
        let proposal = patch
            .inspect_validation(transaction, &plan)
            .unwrap()
            .identity;
        let token = EvaluationToken::approve(
            &proposal,
            &plan,
            "fixture owner",
            "controlled observed execution",
        )
        .unwrap();
        let run = transaction
            .reserve_evaluation(token, &proposal, &plan)
            .unwrap();
        receipt.run_id = run;
        receipt.proposal = proposal.digest();
        receipt.approved_proposal = proposal.clone();
        receipt.plan = plan.digest().unwrap();
        receipt.approved_plan = plan.clone();
        receipt.checks[0].command = plan.checks[0].clone();
        if failed {
            receipt.state = ValidationState::Failed;
            receipt.checks[0].end = validation::ValidationCommandEnd::Exited { code: 1 };
        }
        patch
            .record_validation(transaction, receipt.clone())
            .unwrap();
        transaction.finish_evaluation(&receipt).unwrap();
        plan
    }

    #[test]
    fn edited_receipt_cannot_keep_an_existing_approval_or_gain_a_fresh_one() {
        let (repo, mut patch) = fixture();
        let mut authority = RepositoryAuthority::open(repo.path()).unwrap();
        let mut transaction = authority.try_transaction().unwrap();
        let plan = observed_plan(&mut patch, &mut transaction);
        let token = approval(&patch, &transaction, &plan);
        patch.provenance.validation.as_mut().unwrap().checks[0].stdout =
            "edited after observation".into();
        assert!(matches!(
            transaction.prepare_promotion(&patch, &plan, token),
            Err(ValidationError::Stale)
        ));
        let transaction = authority.try_transaction().unwrap();
        let fresh_token = approval(&patch, &transaction, &plan);
        assert!(matches!(
            transaction.prepare_promotion(&patch, &plan, fresh_token),
            Err(ValidationError::Stale)
        ));
    }

    #[test]
    fn a_completed_failed_run_cannot_be_relabelled_passed_in_receipt_data() {
        let (repo, mut patch) = fixture();
        let mut authority = RepositoryAuthority::open(repo.path()).unwrap();
        let mut transaction = authority.try_transaction().unwrap();
        let plan = observed_plan_result(&mut patch, &mut transaction, true);
        let receipt = patch.provenance.validation.as_mut().unwrap();
        receipt.state = ValidationState::Passed;
        receipt.checks[0].end = validation::ValidationCommandEnd::Exited { code: 0 };
        let token = approval(&patch, &transaction, &plan);
        assert!(matches!(
            transaction.prepare_promotion(&patch, &plan, token),
            Err(ValidationError::NotValidated(ValidationState::Failed))
        ));
    }

    pub(super) fn approval(
        patch: &ProposedPatch,
        transaction: &RepositoryTransaction<'_>,
        plan: &ValidationPlanIdentity,
    ) -> PatchApprovalToken {
        let current = patch.inspect_validation(transaction, plan).unwrap();
        PatchApprovalToken::approve(
            &current.identity,
            plan,
            patch.provenance().validation.as_ref().unwrap(),
            "fixture owner",
            "promote exactly the controlled source",
        )
        .unwrap()
    }

    #[test]
    fn proof_and_post_effect_checkpoint_hold_the_common_lock_through_state_persistence() {
        let (repo, mut patch) = fixture();
        let mut authority = RepositoryAuthority::open(repo.path()).unwrap();
        let mut transaction = authority.try_transaction().unwrap();
        let plan = observed_plan(&mut patch, &mut transaction);
        let token = approval(&patch, &transaction, &plan);
        let proof = transaction.prepare_promotion(&patch, &plan, token).unwrap();
        let mut contender = RepositoryAuthority::open(repo.path()).unwrap();
        assert!(matches!(
            contender.try_transaction(),
            Err(ValidationError::Busy)
        ));
        let pending = patch.promote(proof).unwrap();
        assert_eq!(
            git::head_commit(repo.path()).unwrap(),
            git::head_commit(patch.worktree_path()).unwrap()
        );
        assert!(matches!(
            contender.try_transaction(),
            Err(ValidationError::Busy)
        ));
        // Simulates the owning caller's atomic associated-state checkpoint while
        // the pending object still owns the common transaction guard.
        let state = repo.path().join(".localpilot/fixture-state.json");
        localpilot_store::atomic_write(&state, b"{\"stage\":\"approved\"}").unwrap();
        assert_eq!(pending.outcome().reviewer, "fixture owner");
        pending.finish().unwrap();
        let observer = contender.try_transaction().unwrap();
        assert_eq!(observer.events().count(), 2);
        assert!(observer.events().all(|event| event.completed));
    }

    #[test]
    fn stale_source_after_capability_creation_never_consumes_promotion_or_changes_main() {
        let (repo, mut patch) = fixture();
        let base = git::head_commit(repo.path()).unwrap();
        let mut authority = RepositoryAuthority::open(repo.path()).unwrap();
        let mut transaction = authority.try_transaction().unwrap();
        let plan = observed_plan(&mut patch, &mut transaction);
        let token = approval(&patch, &transaction, &plan);
        let proof = transaction.prepare_promotion(&patch, &plan, token).unwrap();
        fs::write(
            patch.worktree_path().join("sample.rs"),
            "changed after proof\n",
        )
        .unwrap();
        assert!(matches!(patch.promote(proof), Err(ValidationError::Stale)));
        assert_eq!(git::head_commit(repo.path()).unwrap(), base);
        let transaction = authority.try_transaction().unwrap();
        assert_eq!(transaction.events().count(), 1);
    }

    #[test]
    fn missing_validation_and_data_only_passing_receipt_cannot_mint_proof() {
        let (repo, mut patch) = fixture();
        let mut authority = RepositoryAuthority::open(repo.path()).unwrap();
        let transaction = authority.try_transaction().unwrap();
        let (_, mut plan, mut receipt) =
            validation::tests::fixture(transaction.repository_identity());
        plan.worktree = fs::canonicalize(patch.worktree_path())
            .unwrap()
            .to_str()
            .unwrap()
            .into();
        let proposal = patch
            .inspect_validation(&transaction, &plan)
            .unwrap()
            .identity;
        receipt.proposal = proposal.digest();
        receipt.approved_proposal = proposal.clone();
        receipt.plan = plan.digest().unwrap();
        receipt.approved_plan = plan.clone();
        let token = PatchApprovalToken::approve(
            &proposal,
            &plan,
            &receipt,
            "fixture owner",
            "controlled refusal",
        )
        .unwrap();
        assert!(matches!(
            transaction.prepare_promotion(&patch, &plan, token),
            Err(ValidationError::NotValidated(ValidationState::NotRun))
        ));
        assert!(patch.provenance().validation.is_none());
        // A caller can deserialize receipt data, but it cannot thereby recreate
        // a completed local execution reservation in the authority journal.
        patch.provenance.validation = Some(receipt.clone());
        crate::write_proposal_meta(repo.path(), patch.id(), patch.provenance()).unwrap();
        let transaction = authority.try_transaction().unwrap();
        let token = PatchApprovalToken::approve(
            &proposal,
            &plan,
            &receipt,
            "fixture owner",
            "controlled data-only refusal",
        )
        .unwrap();
        assert!(matches!(
            transaction.prepare_promotion(&patch, &plan, token),
            Err(ValidationError::RecoveryRequired)
        ));
    }

    #[test]
    fn proof_for_one_proposal_cannot_promote_another_worktree() {
        let (repo, mut patch) = fixture();
        let base = git::head_commit(repo.path()).unwrap();
        let other = crate::propose(
            repo.path(),
            "other-fixture",
            &PatchProposal::new(
                "different proposal",
                vec!["sample.rs".into()],
                vec![ProposedEdit::new(
                    "sample.rs",
                    "pub fn value() -> u32 { 3 }\n",
                )],
            ),
            ChangeProvenance::new("fixture", "scripted", "wrong object refusal"),
        )
        .unwrap();
        let mut authority = RepositoryAuthority::open(repo.path()).unwrap();
        let mut transaction = authority.try_transaction().unwrap();
        let plan = observed_plan(&mut patch, &mut transaction);
        let token = approval(&patch, &transaction, &plan);
        let proof = transaction.prepare_promotion(&patch, &plan, token).unwrap();
        assert!(matches!(other.promote(proof), Err(ValidationError::Stale)));
        assert_eq!(git::head_commit(repo.path()).unwrap(), base);
        assert_eq!(authority.try_transaction().unwrap().events().count(), 1);
    }

    #[test]
    #[ignore = "subprocess helper invoked by the actual promotion crash fixture"]
    fn promotion_crash_worker() {
        let Some(root) = std::env::var_os("LOCALPILOT_PROMOTION_CRASH_ROOT") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        let patch = ProposedPatch::reopen(&root, "validated-fixture").unwrap();
        let plan: ValidationPlanIdentity = serde_json::from_slice(
            &fs::read(root.join(".localpilot/promotion-fixture-plan.json")).unwrap(),
        )
        .unwrap();
        let mut authority = RepositoryAuthority::open(&root).unwrap();
        let transaction = authority.try_transaction().unwrap();
        let token = approval(&patch, &transaction, &plan);
        let proof = transaction.prepare_promotion(&patch, &plan, token).unwrap();
        let boundary = std::env::var("LOCALPILOT_PROMOTION_CRASH_BOUNDARY").unwrap();
        if boundary == "after_proof" {
            std::process::exit(75);
        }
        let pending = if boundary == "before_git" {
            patch
                .promote_after_reservation(proof, || std::process::exit(75))
                .unwrap()
        } else {
            patch.promote(proof).unwrap()
        };
        if matches!(boundary.as_str(), "after_state" | "after_finish") {
            localpilot_store::atomic_write(
                &root.join(".localpilot/fixture-state.json"),
                b"{\"stage\":\"approved\"}",
            )
            .unwrap();
        }
        if boundary == "after_finish" {
            pending.finish().unwrap();
        }
        // Skip Rust destructors at the exact Git-effect/associated-state gap.
        std::process::exit(75);
    }

    fn assert_process_boundary(boundary: &str) {
        let (repo, mut patch) = fixture();
        let base = git::head_commit(repo.path()).unwrap();
        let proposed = git::head_commit(patch.worktree_path()).unwrap();
        let mut authority = RepositoryAuthority::open(repo.path()).unwrap();
        let mut transaction = authority.try_transaction().unwrap();
        let plan = observed_plan(&mut patch, &mut transaction);
        drop(transaction);
        drop(authority);
        fs::write(
            repo.path().join(".localpilot/promotion-fixture-plan.json"),
            serde_json::to_vec(&plan).unwrap(),
        )
        .unwrap();
        patch.persist().unwrap();
        let log = fs::File::create(repo.path().join(".localpilot/promotion-worker.log")).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "validation::promotion::tests::promotion_crash_worker",
                "--ignored",
                "--nocapture",
            ])
            .env("LOCALPILOT_PROMOTION_CRASH_ROOT", repo.path())
            .env("LOCALPILOT_PROMOTION_CRASH_BOUNDARY", boundary)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("promotion worker did not exit within its bound");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert_eq!(
            status.code(),
            Some(75),
            "{}",
            fs::read_to_string(repo.path().join(".localpilot/promotion-worker.log")).unwrap()
        );
        let changed = !matches!(boundary, "after_proof" | "before_git");
        assert_eq!(
            git::head_commit(repo.path()).unwrap(),
            if changed {
                proposed.clone()
            } else {
                base.clone()
            }
        );
        assert_eq!(
            repo.path().join(".localpilot/fixture-state.json").exists(),
            matches!(boundary, "after_state" | "after_finish")
        );
        let mut authority = RepositoryAuthority::open(repo.path()).unwrap();
        let mut transaction = authority.try_transaction().unwrap();
        if boundary == "after_proof" {
            assert_eq!(transaction.events().count(), 1);
            assert!(transaction.events().all(|event| event.completed));
            return;
        }
        let promotion = transaction
            .events()
            .find(|event| event.promotion.is_some())
            .unwrap();
        let goal = promotion.promotion.as_ref().unwrap();
        assert_eq!(goal.base_commit, base);
        assert_eq!(goal.proposed_commit, proposed);
        assert_eq!(goal.target_ref, "refs/heads/main");
        assert_eq!(goal.proposal_id, "validated-fixture");
        if boundary == "after_finish" {
            assert!(transaction.events().all(|event| event.completed));
            return;
        }
        assert!(!promotion.completed);
        let mut proposal = validation::tests::fixture(transaction.repository_identity()).0;
        proposal.repository = transaction.repository_identity();
        let token = EvaluationToken::approve(
            &proposal,
            &plan,
            "fixture owner",
            "controlled new-action refusal",
        )
        .unwrap();
        assert!(matches!(
            transaction.reserve_evaluation(token, &proposal, &plan),
            Err(ValidationError::RecoveryRequired)
        ));
    }

    #[test]
    fn process_crash_after_proof_before_reservation_changes_no_source_or_state() {
        assert_process_boundary("after_proof");
    }

    #[test]
    fn process_crash_after_reservation_before_git_keeps_the_goal_pending() {
        assert_process_boundary("before_git");
    }

    #[test]
    fn process_crash_after_git_before_state_keeps_the_goal_pending() {
        assert_process_boundary("after_git");
    }

    #[test]
    fn process_crash_after_state_before_finish_keeps_the_goal_pending() {
        assert_process_boundary("after_state");
    }

    #[test]
    fn process_crash_after_final_checkpoint_has_consistent_source_state_and_journal() {
        assert_process_boundary("after_finish");
    }
}
