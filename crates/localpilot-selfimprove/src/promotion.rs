//! Common locked promotion service for direct and orchestrated hosts.

use crate::orchestrator::{clear_loop_state, read_loop_state, write_loop_state};
use crate::preflight::{preflight_in_transaction, PreflightReport};
use crate::{LoopError, Stage};
use localpilot_patchgen::validation::{
    CallerCheckpoint, ContentDigest, PatchApprovalToken, RepositoryAuthority,
    RepositoryTransaction, ValidationError, ValidationEvidence, ValidationState,
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

/// Remove a proposal under the common guard and clear a matching unpromoted
/// loop. Completed loop stages and historical run receipts are preserved.
///
/// # Errors
/// Busy/pending authority, missing proposal, removal or state cleanup failure.
pub fn discard_proposal(root: &Path, id: &str) -> Result<(), LoopError> {
    let mut authority = RepositoryAuthority::open(root)?;
    let transaction = authority.try_transaction()?;
    transaction.require_idle()?;
    let state = read_loop_state(root)?;
    ProposedPatch::reopen(root, id)?.discard_in_transaction(&transaction)?;
    if state.as_ref().is_some_and(|state| {
        state.stage == Stage::Proposed && state.proposal_id.as_deref() == Some(id)
    }) {
        clear_loop_state(root)?;
    }
    Ok(())
}

pub(crate) fn promote_for_loop(
    root: &Path,
    id: &str,
    review: &PromotionReview,
    token: PatchApprovalToken,
    require_loop: bool,
) -> Result<PromoteOutcome, LoopError> {
    promote_at_boundaries(root, id, review, token, require_loop, &mut |_| {})
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromotionBoundary {
    BeforeSource,
    AfterSource,
    AfterState,
    AfterFinish,
}

// Private observation seam: production supplies no behavior. Tests terminate a
// real process at these boundaries; no runtime flag or environment hook exists.
fn promote_at_boundaries(
    root: &Path,
    id: &str,
    review: &PromotionReview,
    token: PatchApprovalToken,
    require_loop: bool,
    checkpoint: &mut impl FnMut(PromotionBoundary),
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
    let caller_checkpoint = if let Some(before) = state.as_ref().filter(|_| matching) {
        let mut after = before.clone();
        after.stage = Stage::Approved;
        after.reviewer = Some(token.reviewer());
        CallerCheckpoint::Required {
            before: loop_state_digest(before)?,
            after: loop_state_digest(&after)?,
        }
    } else {
        CallerCheckpoint::None
    };
    let proof = transaction
        .prepare_promotion(&patch, &current.preflight.plan, token)?
        .with_caller_checkpoint(caller_checkpoint);
    checkpoint(PromotionBoundary::BeforeSource);
    let pending = patch.promote(proof)?;
    checkpoint(PromotionBoundary::AfterSource);
    if let Some(state) = state.as_mut().filter(|_| matching) {
        state.stage = Stage::Approved;
        state.reviewer = Some(pending.outcome().reviewer.clone());
        write_loop_state(root, state)?;
    }
    checkpoint(PromotionBoundary::AfterState);
    let outcome = pending.finish()?;
    checkpoint(PromotionBoundary::AfterFinish);
    Ok(outcome)
}

pub(crate) fn loop_state_digest(state: &crate::LoopState) -> Result<ContentDigest, LoopError> {
    let json = serde_json::to_vec(state)
        .map_err(|_| LoopError::Serde("loop state identity cannot be encoded".into()))?;
    Ok(ContentDigest::for_bytes(&json))
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

    #[tokio::test]
    async fn approved_host_access_is_visible_and_real_build_gauntlet_and_rollback_remain_separate()
    {
        let sentinel = tempfile::tempdir().unwrap();
        let sentinel_root = sentinel.path().to_path_buf();
        let marker = sentinel_root.join("approved-host-effect");
        let source = format!(
            "#[test] fn host_access() {{ std::fs::write({:?}, b\"explicit fixture effect\").unwrap(); }}\n",
            marker.to_str().unwrap()
        );
        let (root, patch, base) = crate::execution::tests::fixture_with_setup(&source, |root| {
            std::fs::write(root.join("Cargo.toml"), "[package]\nname='localpilot'\nversion='0.0.0'\nedition='2021'\n[profile.selfdev]\ninherits='release'\nopt-level=0\n").unwrap();
            std::fs::write(
                root.join("Cargo.lock"),
                "version = 3\n[[package]]\nname = \"localpilot\"\nversion = \"0.0.0\"\n",
            )
            .unwrap();
            // A compiled local protocol stand-in, not the actual LocalPilot RPC
            // implementation. Exercise the real builder, gauntlet and store.
            std::fs::write(root.join("src/main.rs"), r#"
use std::io::{BufRead, Write};
fn main() {
    if std::env::args().nth(1).as_deref() == Some("version") {
        println!("{{\"version\":\"{}\",\"git_hash\":\"{}\",\"fingerprint\":\"{}\"}}", env!("LOCALPILOT_VERSION"), env!("LOCALPILOT_GIT_HASH"), env!("LOCALPILOT_SOURCE_FINGERPRINT"));
    } else {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).unwrap();
        assert!(line.contains("hello"));
        println!("{{\"v\":1,\"event\":{{\"type\":\"hello\"}}}}");
        std::io::stdout().flush().unwrap();
    }
}
"#).unwrap();
        });
        let selfdev = tempfile::tempdir().unwrap();
        let mut loop_ = Orchestrator::open(
            root.path(),
            selfdev.path(),
            crate::SelfDevRunner::new(selfdev.path()),
        );
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
        let preview = crate::proposal_preflight(root.path(), patch.id()).unwrap();
        assert!(preview.render().contains("host"));
        assert!(!marker.exists());
        assert!(promotion_review(root.path(), patch.id()).is_err());
        let receipt = crate::execution::tests::run(
            root.path(),
            &patch,
            Profile::Default,
            CancelSignal::new(),
        )
        .await;
        assert_eq!(receipt.state, ValidationState::Passed);
        assert!(receipt
            .checks
            .iter()
            .all(|check| check.cleanup_confirmed && check.pipes_closed));
        assert_eq!(std::fs::read(&marker).unwrap(), b"explicit fixture effect");
        let current = localpilot_selfdev::SourceState::read(root.path()).unwrap();
        assert_eq!(current.head.as_deref(), Some(base.as_str()));
        assert!(!current.dirty);
        assert_eq!(loop_.state().unwrap().unwrap().stage, Stage::Proposed);
        let review = promotion_review(root.path(), patch.id()).unwrap();
        loop_.approve(&review, consent(&review)).unwrap();
        assert_eq!(loop_.state().unwrap().unwrap().stage, Stage::Approved);
        let channels = localpilot_selfdev::Channels::new(selfdev.path());
        channels
            .set(localpilot_selfdev::CURRENT, "prior-known-good")
            .unwrap();
        let built = loop_.build(&mut Vec::new()).unwrap();
        assert_eq!(loop_.state().unwrap().unwrap().stage, Stage::Built);
        assert_eq!(
            channels.label(localpilot_selfdev::CURRENT).as_deref(),
            Some(built.label.as_str())
        );
        let installed = localpilot_selfdev::VersionStore::new(selfdev.path())
            .get(&built.label)
            .unwrap();
        let mut wrong = localpilot_selfdev::SourceState::read(root.path()).unwrap();
        wrong.fingerprint = "wrong-source-fixture".into();
        let scratch = tempfile::tempdir().unwrap();
        assert!(localpilot_selfdev::vet(
            &installed.executable(),
            &wrong,
            scratch.path(),
            std::time::Duration::from_secs(5)
        )
        .is_err());
        // Inject a failed activation and exercise the existing real rollback
        // primitive. Source promotion and runtime channel activation differ.
        channels
            .set(localpilot_selfdev::CURRENT, "prior-known-good")
            .unwrap();
        let guard = localpilot_selfdev::ActivationGuard::new(selfdev.path());
        guard
            .begin(
                &channels,
                "fixture-activation",
                localpilot_selfdev::CURRENT.into(),
                &built.label,
            )
            .unwrap();
        channels
            .set(localpilot_selfdev::CURRENT, &built.label)
            .unwrap();
        assert!(guard.roll_back(&channels, "fixture-activation").unwrap());
        assert_eq!(
            channels.label(localpilot_selfdev::CURRENT).as_deref(),
            Some("prior-known-good")
        );
        assert!(guard.pending("fixture-activation").is_none());
        assert_eq!(loop_.state().unwrap().unwrap().stage, Stage::Built);
        sentinel.close().unwrap();
        assert!(
            !sentinel_root.exists(),
            "owned host-effect sentinel must be removed"
        );
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

    struct Worker(std::process::Child);
    impl Drop for Worker {
        fn drop(&mut self) {
            if !matches!(self.0.try_wait(), Ok(Some(_))) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }
    impl Worker {
        fn wait(&mut self, deadline: std::time::Instant) -> std::process::ExitStatus {
            loop {
                if let Some(status) = self.0.try_wait().unwrap() {
                    return status;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "common promotion worker exceeded its deadline"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
    fn worker(root: &Path, id: &str, mode: &str) -> Worker {
        Worker(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "promotion::tests::common_promotion_worker",
                    "--ignored",
                    "--nocapture",
                ])
                .env("LOCALPILOT_TEST_COMMON_ROOT", root)
                .env("LOCALPILOT_TEST_COMMON_ID", id)
                .env("LOCALPILOT_TEST_COMMON_MODE", mode)
                .spawn()
                .unwrap(),
        )
    }
    fn wait_marker(path: &Path, deadline: std::time::Instant) {
        while !path.is_file() {
            assert!(
                std::time::Instant::now() < deadline,
                "common promotion worker did not reach its boundary"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    fn seed_proposed(root: &Path, id: &str) {
        write_loop_state(
            root,
            &LoopState {
                stage: Stage::Proposed,
                proposal_id: Some(id.into()),
                built_label: None,
                reviewer: None,
            },
        )
        .unwrap();
    }

    #[test]
    #[ignore = "actual subprocess helper for common promotion boundaries"]
    fn common_promotion_worker() {
        let root =
            std::path::PathBuf::from(std::env::var_os("LOCALPILOT_TEST_COMMON_ROOT").unwrap());
        let id = std::env::var("LOCALPILOT_TEST_COMMON_ID").unwrap();
        let mode = std::env::var("LOCALPILOT_TEST_COMMON_MODE").unwrap();
        let review = promotion_review(&root, &id).unwrap();
        let token = consent(&review);
        if mode == "reserved" {
            let mut authority = RepositoryAuthority::open(&root).unwrap();
            let mut tx = authority.try_transaction().unwrap();
            let before = read_loop_state(&root).unwrap().unwrap();
            let mut after = before.clone();
            after.stage = Stage::Approved;
            after.reviewer = Some(token.reviewer());
            tx.reserve_promotion_with_checkpoint(
                token,
                &review.preflight.proposal.identity,
                &review.preflight.plan,
                &review.evidence,
                CallerCheckpoint::Required {
                    before: loop_state_digest(&before).unwrap(),
                    after: loop_state_digest(&after).unwrap(),
                },
            )
            .unwrap();
            std::process::exit(77);
        }
        if mode == "direct" || mode == "orchestrated" {
            std::fs::write(
                root.join(format!(".localpilot/{mode}.ready")),
                "review prepared",
            )
            .unwrap();
            wait_marker(
                &root.join(".localpilot/promote.go"),
                std::time::Instant::now() + std::time::Duration::from_secs(120),
            );
            let result = promote_for_loop(&root, &id, &review, token, mode == "orchestrated");
            let result = match result {
                Ok(_) => "promoted",
                Err(LoopError::Validation(ValidationError::Busy | ValidationError::Stale))
                | Err(LoopError::OutOfOrder { .. }) => "refused",
                Err(error) => panic!("unexpected common promotion outcome: {error}"),
            };
            std::fs::write(root.join(format!(".localpilot/{mode}.result")), result).unwrap();
            return;
        }
        let boundary = match mode.as_str() {
            "before_source" => PromotionBoundary::BeforeSource,
            "after_source" => PromotionBoundary::AfterSource,
            "after_state" => PromotionBoundary::AfterState,
            _ => panic!("unknown controlled common promotion boundary"),
        };
        promote_at_boundaries(&root, &id, &review, token, true, &mut |observed| {
            if observed == boundary {
                std::process::exit(77);
            }
        })
        .unwrap();
        panic!("controlled common promotion boundary was not reached");
    }

    async fn common_crash_recovery(mode: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
        let (root, patch, base) = crate::execution::tests::fixture("#[test] fn works() {}\n");
        assert_eq!(
            crate::execution::tests::run(
                root.path(),
                &patch,
                Profile::Default,
                CancelSignal::new()
            )
            .await
            .state,
            ValidationState::Passed
        );
        seed_proposed(root.path(), patch.id());
        let mut child = worker(root.path(), patch.id(), mode);
        assert_eq!(child.wait(deadline).code(), Some(77));
        let events = crate::pending_authority(root.path()).unwrap();
        if mode == "before_source" {
            assert!(events.is_empty());
            assert_eq!(
                read_loop_state(root.path()).unwrap().unwrap().stage,
                Stage::Proposed
            );
            assert_eq!(
                promotion_review(root.path(), patch.id())
                    .unwrap()
                    .preflight
                    .proposal
                    .identity
                    .base_commit,
                base
            );
            return;
        }
        assert_eq!(events.len(), 1);
        let nonce = events[0].reservation.nonce;
        let review = crate::recovery_review(root.path(), nonce).unwrap();
        assert!(matches!(
            review.event.promotion.as_ref().unwrap().caller_checkpoint,
            CallerCheckpoint::Required { .. }
        ));
        assert_eq!(
            review.loop_state.as_ref().unwrap().stage,
            if mode == "after_state" {
                Stage::Approved
            } else {
                Stage::Proposed
            }
        );
        assert!(crate::acknowledge_recovery(root.path(), &review, "owner", "").is_err());
        let mut stale = review.clone();
        stale.event.reservation.intent.push_str("changed review");
        assert!(matches!(
            crate::acknowledge_recovery(
                root.path(),
                &stale,
                "owner",
                "inspected exact source and state"
            ),
            Err(LoopError::Validation(ValidationError::Stale))
        ));

        // Even a freshly displayed review cannot retire inconsistent caller
        // state, missing state or a dirty source tree.
        let original = read_loop_state(root.path()).unwrap().unwrap();
        let mut changed = original.clone();
        changed.built_label = Some("unexpected-state".into());
        write_loop_state(root.path(), &changed).unwrap();
        let bad = crate::recovery_review(root.path(), nonce).unwrap();
        assert!(
            crate::acknowledge_recovery(root.path(), &bad, "owner", "inspected changed state")
                .is_err()
        );
        crate::orchestrator::clear_loop_state(root.path()).unwrap();
        let missing = crate::recovery_review(root.path(), nonce).unwrap();
        assert!(crate::acknowledge_recovery(
            root.path(),
            &missing,
            "owner",
            "inspected missing state"
        )
        .is_err());
        write_loop_state(root.path(), &original).unwrap();
        let dirty = root.path().join("recovery-untracked-control");
        std::fs::write(&dirty, "must not accept dirty main").unwrap();
        let bad = crate::recovery_review(root.path(), nonce).unwrap();
        assert!(
            crate::acknowledge_recovery(root.path(), &bad, "owner", "inspected dirty source")
                .is_err()
        );
        std::fs::remove_file(dirty).unwrap();
        let current = crate::recovery_review(root.path(), nonce).unwrap();
        crate::acknowledge_recovery(
            root.path(),
            &current,
            "owner",
            "inspected processes and exact journaled source/state; no replay",
        )
        .unwrap();
        assert!(crate::pending_authority(root.path()).unwrap().is_empty());
        assert_eq!(
            read_loop_state(root.path()).unwrap().unwrap().stage,
            if mode == "reserved" {
                Stage::Proposed
            } else {
                Stage::Approved
            }
        );
        assert!(matches!(
            crate::acknowledge_recovery(root.path(), &current, "owner", "repeat"),
            Err(LoopError::Validation(ValidationError::Consumed))
        ));
    }

    #[tokio::test]
    async fn common_crash_before_source_keeps_the_validated_loop_unpromoted() {
        common_crash_recovery("before_source").await;
    }
    #[tokio::test]
    async fn common_reserved_intent_recovers_without_replaying_source_or_advancing_state() {
        common_crash_recovery("reserved").await;
    }
    #[tokio::test]
    async fn common_crash_after_source_reconciles_only_the_exact_journaled_state() {
        common_crash_recovery("after_source").await;
    }
    #[tokio::test]
    async fn common_crash_after_state_retires_the_intent_without_a_second_source_effect() {
        common_crash_recovery("after_state").await;
    }

    #[tokio::test]
    async fn concurrent_direct_and_orchestrated_processes_cannot_both_promote() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
        let (root, patch, _) = crate::execution::tests::fixture("#[test] fn works() {}\n");
        assert_eq!(
            crate::execution::tests::run(
                root.path(),
                &patch,
                Profile::Default,
                CancelSignal::new()
            )
            .await
            .state,
            ValidationState::Passed
        );
        seed_proposed(root.path(), patch.id());
        let mut direct = worker(root.path(), patch.id(), "direct");
        wait_marker(&root.path().join(".localpilot/direct.ready"), deadline);
        let mut orchestrated = worker(root.path(), patch.id(), "orchestrated");
        wait_marker(
            &root.path().join(".localpilot/orchestrated.ready"),
            deadline,
        );
        std::fs::write(
            root.path().join(".localpilot/promote.go"),
            "two independently confirmed attempts",
        )
        .unwrap();
        assert!(direct.wait(deadline).success());
        assert!(orchestrated.wait(deadline).success());
        let mut outcomes = [
            std::fs::read_to_string(root.path().join(".localpilot/direct.result")).unwrap(),
            std::fs::read_to_string(root.path().join(".localpilot/orchestrated.result")).unwrap(),
        ];
        outcomes.sort();
        assert_eq!(outcomes, ["promoted", "refused"]);
        assert_eq!(
            read_loop_state(root.path()).unwrap().unwrap().stage,
            Stage::Approved
        );
        let mut authority = RepositoryAuthority::open(root.path()).unwrap();
        let tx = authority.try_transaction().unwrap();
        assert_eq!(tx.events().count(), 2);
        assert!(tx.events().all(|event| event.completed));
    }

    #[test]
    fn discard_obeys_the_common_guard_and_clears_only_a_matching_unpromoted_loop() {
        let (root, patch, _) = crate::execution::tests::fixture("#[test] fn works() {}\n");
        seed_proposed(root.path(), patch.id());
        let mut authority = RepositoryAuthority::open(root.path()).unwrap();
        let held = authority.try_transaction().unwrap();
        assert!(matches!(
            discard_proposal(root.path(), patch.id()),
            Err(LoopError::Validation(ValidationError::Busy))
        ));
        assert!(patch.worktree_path().is_dir());
        drop(held);
        discard_proposal(root.path(), patch.id()).unwrap();
        assert!(!patch.worktree_path().exists());
        assert!(read_loop_state(root.path()).unwrap().is_none());
    }
}
