#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use proptest::prelude::*;

proptest! {
    #[test]
    fn canonical_hash_keeps_field_boundaries_and_round_trips(
        first in proptest::collection::vec(any::<u8>(), 0..64),
        second in proptest::collection::vec(any::<u8>(), 0..64),
    ) {
        let pair = ContentDigest::hash("property", &[&first, &second]);
        let combined = [first, second].concat();
        prop_assert_ne!(&pair, &ContentDigest::hash("property", &[&combined]));
        let restored: ContentDigest = serde_json::from_str(&serde_json::to_string(&pair).unwrap()).unwrap();
        prop_assert_eq!(restored, pair);
    }
}

pub(super) fn fixture(
    repository: ContentDigest,
) -> (ProposalIdentity, ValidationPlanIdentity, ValidationEvidence) {
    let content = ContentDigest::hash("fixture-content", &[b"bytes"]);
    let proposal = ProposalIdentity {
        repository,
        proposal_id: "proposal-1".into(),
        target_ref: "refs/heads/main".into(),
        base_commit: "a".repeat(40),
        proposed_commit: "b".repeat(40),
        proposed_tree: "c".repeat(40),
        source_bytes: content.clone(),
        diff: content.clone(),
        provenance: content.clone(),
        text_policy: content.clone(),
    };
    let root = if cfg!(windows) {
        "C:/validation"
    } else {
        "/validation"
    };
    let plan = ValidationPlanIdentity {
        profile: "trusted-check-v1".into(),
        policy_version: "1".into(),
        platform: std::env::consts::OS.into(),
        worktree: root.into(),
        toolchain: content.clone(),
        checks: vec![PlannedCheck {
            name: "test".into(),
            program: format!("{root}/host/check"),
            executable: content,
            args: vec!["--frozen".into(), "a b".into()],
        }],
        environment_names: Vec::new(),
        artifact_paths: Vec::new(),
        limits: ValidationLimits::default(),
    };
    let now = super::authority::unix_now().unwrap();
    let receipt = ValidationEvidence {
        schema: VALIDATION_SCHEMA.into(),
        run_id: uuid::Uuid::new_v4(),
        proposal: proposal.digest(),
        plan: plan.digest().unwrap(),
        state: ValidationState::Passed,
        started_at: now,
        finished_at: now,
        checks: vec![ValidationCheckEvidence {
            command: plan.checks[0].clone(),
            end: ValidationCommandEnd::Exited { code: 0 },
            elapsed_ms: 1,
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
            pipes_closed: true,
            cleanup_confirmed: true,
        }],
        artifacts: Vec::new(),
    };
    (proposal, plan, receipt)
}

#[test]
fn canonical_fields_preserve_boundaries_and_purpose() {
    assert_ne!(
        ContentDigest::hash("test", &[b"ab", b"c"]),
        ContentDigest::hash("test", &[b"a", b"bc"])
    );
    assert_ne!(
        ContentDigest::hash("test", &[b"ab"]),
        ContentDigest::hash("other", &[b"ab"])
    );
    assert_ne!(
        ContentDigest::hash("test", &[b""]),
        ContentDigest::hash("test", &[])
    );
    // Golden byte framing independent of filesystem, path display or platform.
    assert_eq!(
        ContentDigest::hash("test", &[b"ab", b"c"]).as_str(),
        "a00f0094952cdea2fc015771273529f056651e3b24bc20f91c65feeaa177825f"
    );
    assert!(ContentDigest::try_from("F".repeat(64)).is_err());
    assert!(serde_json::from_str::<ContentDigest>("\"short\"").is_err());
}

#[test]
fn proposal_and_plan_changes_invalidate_passing_evidence() {
    let (proposal, plan, receipt) = fixture(ContentDigest::hash("repo", &[b"one"]));
    let now = receipt.finished_at;
    assert!(receipt.verify_current(&proposal, &plan, now).is_ok());
    let mut changed = proposal.clone();
    changed.base_commit = "d".repeat(40);
    assert!(matches!(
        receipt.verify_current(&changed, &plan, now),
        Err(ValidationError::Stale)
    ));
    changed = proposal.clone();
    changed.repository = ContentDigest::hash("repo", &[b"two"]);
    assert!(matches!(
        receipt.verify_current(&changed, &plan, now),
        Err(ValidationError::Stale)
    ));
    changed = proposal.clone();
    changed.source_bytes = ContentDigest::hash("CRLF", &[b"different checkout bytes"]);
    assert!(matches!(
        receipt.verify_current(&changed, &plan, now),
        Err(ValidationError::Stale)
    ));
    let mut changed_plan = plan.clone();
    changed_plan.checks[0].args = vec!["--frozen".into(), "a".into(), "b".into()];
    assert!(matches!(
        receipt.verify_current(&proposal, &changed_plan, now),
        Err(ValidationError::Stale)
    ));
    changed_plan = plan.clone();
    changed_plan.checks[0].executable = ContentDigest::hash("program", &[b"replaced"]);
    assert!(matches!(
        receipt.verify_current(&proposal, &changed_plan, now),
        Err(ValidationError::Stale)
    ));
    changed_plan = plan.clone();
    changed_plan.policy_version.push('2');
    assert!(matches!(
        receipt.verify_current(&proposal, &changed_plan, now),
        Err(ValidationError::Stale)
    ));
}

#[test]
fn incomplete_cleanup_partial_and_expired_receipts_never_pass() {
    let (proposal, plan, receipt) = fixture(ContentDigest::hash("repo", &[]));
    let now = receipt.finished_at;
    for replacement in [
        ValidationCommandEnd::Exited { code: 1 },
        ValidationCommandEnd::Denied,
        ValidationCommandEnd::NotStarted,
        ValidationCommandEnd::TimedOut,
        ValidationCommandEnd::Cancelled,
        ValidationCommandEnd::Failed,
    ] {
        let mut incomplete = receipt.clone();
        incomplete.checks[0].end = replacement;
        assert!(incomplete.verify_current(&proposal, &plan, now).is_err());
    }
    for kind in 0..6 {
        let mut incomplete = receipt.clone();
        match kind {
            0 => incomplete.checks[0].cleanup_confirmed = false,
            1 => incomplete.checks[0].pipes_closed = false,
            2 => incomplete.checks.clear(),
            3 => incomplete.checks[0].stdout = "x".repeat(16 * 1024 + 1),
            4 => incomplete.schema.push('2'),
            _ => incomplete.run_id = uuid::Uuid::nil(),
        }
        assert!(incomplete.verify_current(&proposal, &plan, now).is_err());
    }
    assert!(matches!(
        receipt.verify_current(&proposal, &plan, now - 1),
        Err(ValidationError::Clock)
    ));
    assert!(receipt
        .verify_current(&proposal, &plan, now + RECEIPT_LIFETIME_SECS - 1)
        .is_ok());
    assert!(matches!(
        receipt.verify_current(&proposal, &plan, now + RECEIPT_LIFETIME_SECS),
        Err(ValidationError::Expired)
    ));
    let mut incomplete = receipt;
    incomplete.checks[0].truncated = true;
    assert!(
        incomplete.verify_current(&proposal, &plan, now).is_ok(),
        "bounded truncation alone is not failure"
    );
}

#[test]
fn unsupported_plans_and_artifact_escape_fail_before_authorization() {
    let (_, plan, _) = fixture(ContentDigest::hash("repo", &[]));
    for path in [
        "../outside",
        "/absolute",
        "C:/outside",
        "a\\b",
        "a//b",
        ".",
        "a/../b",
    ] {
        let mut changed = plan.clone();
        changed.artifact_paths = vec![path.into()];
        assert!(changed.digest().is_err(), "{path}");
    }
    for mutation in 0..5 {
        let mut changed = plan.clone();
        match mutation {
            0 => changed.checks.clear(),
            1 => changed.limits.total_secs += 1,
            2 => changed.environment_names = vec!["Z".into(), "A".into()],
            3 => changed.checks[0].program = "relative-check".into(),
            _ => {
                changed.checks[0].program = if cfg!(windows) {
                    "C:/host/pwsh.exe"
                } else {
                    "/bin/sh"
                }
                .into()
            }
        }
        assert!(changed.digest().is_err());
    }
}

#[test]
fn legacy_provenance_is_not_validated_and_interrupted_runs_do_not_resume() {
    let legacy = r#"{"schema":"localpilot-change-provenance-v1","prompt":"p","model":"m","tools_used":[],"test_evidence":"old test","rationale":"r","risks":"","rollback_notes":"discard","lessons":[],"eval_result":{"passed":true,"summary":"legacy"}}"#;
    let provenance: crate::ChangeProvenance = serde_json::from_str(legacy).unwrap();
    assert!(provenance.validation.is_none());
    assert!(!provenance.to_json().unwrap().contains("validation"));
    let (_, _, mut receipt) = fixture(ContentDigest::hash("repo", &[]));
    for state in [ValidationState::Authorized, ValidationState::Running] {
        receipt.state = state;
        let resumed: ValidationEvidence =
            serde_json::from_str(&serde_json::to_string(&receipt).unwrap()).unwrap();
        assert_eq!(resumed.resumed_state(), ValidationState::Invalid);
    }
    receipt.schema = "unknown".into();
    receipt.state = ValidationState::Passed;
    assert_eq!(receipt.resumed_state(), ValidationState::Invalid);
}

#[test]
fn execution_consent_expires_on_environment_changes_and_debug_is_redacted() {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init"]);
    let mut authority = RepositoryAuthority::open(repo.path()).unwrap();
    let mut transaction = authority.try_transaction().unwrap();
    let (proposal, mut plan, _) = fixture(transaction.repository_identity());
    let key = format!("LOCALPILOT_FIXTURE_{}", uuid::Uuid::new_v4().simple());
    std::env::set_var(&key, "initial-private-value");
    plan.environment_names = vec![key.clone()];
    let token = EvaluationToken::approve(&proposal, &plan, "reviewer", "run fixed plan").unwrap();
    assert_eq!(format!("{token:?}"), "execution consent [redacted]");
    std::env::set_var(&key, "changed-private-value");
    assert!(matches!(
        transaction.reserve_evaluation(token, &proposal, &plan),
        Err(ValidationError::Stale)
    ));
    std::env::remove_var(key);
    assert_eq!(
        transaction.events().count(),
        0,
        "stale authority never reserves effects"
    );
}

pub(super) fn git(root: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            &format!(
                "core.hooksPath={}",
                root.join(".git/fixture-empty-hooks").display()
            ),
        ])
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
