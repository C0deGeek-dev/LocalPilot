//! Compose the existing execute-only runner with proposal-owned durable consent.
//! An abandoned future leaves an unfinished journal entry, never reusable consent.

use crate::preflight::{preflight_in_transaction, PreflightReport};
use localpilot_config::{AutoFix, CheckConfig};
use localpilot_harness::{CancelSignal, CheckRunner, CleanupOutcome, CommandEnd, EnvPolicy};
use localpilot_patchgen::validation::{
    EvaluationToken, RepositoryAuthority, ValidationCheckEvidence, ValidationCommandEnd,
    ValidationError, ValidationEvidence, ValidationState, VALIDATION_SCHEMA,
};
use localpilot_patchgen::ProposedPatch;
use localpilot_sandbox::{
    Approver, Effect, ExactCommand, Interactivity, PermissionEngine, PermissionRequest, Workspace,
};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// The shared runner bounds stop/confirm hooks to 5s each, wait to 2s and the
// two pipes to 2s each. Reserve that cleanup time inside the approved deadline.
const CLEANUP_RESERVE: Duration = Duration::from_secs(16);

/// Execute a previously reviewed proposal using fresh, purpose-specific consent.
/// The caller obtains consent for `review` before issuing `token`. Each command
/// still traverses the ordinary permission engine; consent answers only its Ask.
/// No fixer, retry, promotion, publication, model or GPU process is requested.
///
/// The exclusive common-repository guard spans identity checks, durable nonce
/// consumption, command execution, final receipt persistence and journal finish.
/// Interruptions and uncertain cleanup require explicit recovery inspection.
///
/// # Errors
/// Stale review, permission/context/identity failures, interrupted authority,
/// failed receipt persistence or output sink. Ordinary observed check failures
/// return a nonpassing receipt. An error after consumption leaves recovery pending.
#[allow(clippy::too_many_arguments)]
pub async fn validate_proposal(
    root: &Path,
    id: &str,
    review: &PreflightReport,
    token: EvaluationToken,
    engine: &PermissionEngine,
    trusted: bool,
    cancel: CancelSignal,
    progress: &mut dyn Write,
) -> Result<ValidationEvidence, ValidationError> {
    let started = Instant::now();
    let mut authority = RepositoryAuthority::open(root)?;
    let mut transaction = authority.try_transaction()?;
    let mut patch = ProposedPatch::reopen(root, id)?;
    let current = preflight_in_transaction(root, &patch, &transaction)?;
    if current.digest()? != review.digest()? || current.plan != review.plan {
        return Err(ValidationError::Stale);
    }
    let workspace = Workspace::new(Path::new(&current.plan.worktree))
        .map_err(|_| invalid("proposal workspace cannot be resolved"))?;
    if workspace.root() != Path::new(&current.plan.worktree) {
        return Err(ValidationError::Stale);
    }
    let plan = &current.plan;
    let run_id = transaction.reserve_evaluation(token, &current.proposal.identity, plan)?;
    let mut receipt = ValidationEvidence {
        schema: VALIDATION_SCHEMA.into(),
        run_id,
        proposal: current.proposal.identity.digest(),
        plan: plan.digest()?,
        approved_plan: plan.clone(),
        state: ValidationState::Running,
        started_at: now()?,
        finished_at: now()?,
        checks: Vec::new(),
        artifacts: Vec::new(),
    };
    patch.record_validation(&transaction, receipt.clone())?;
    let exact: Vec<_> = plan
        .checks
        .iter()
        .map(|check| ExactCommand {
            program: check.program.clone(),
            args: check.args.clone(),
        })
        .collect();
    let approver = ConsumedPlanApprover {
        commands: &exact,
        trusted,
    };
    let mut safely_stopped = true;
    for check in &plan.checks {
        // Re-resolve from original base configuration under the same lock at
        // every execution boundary, including after a previous check ran code.
        let fresh = preflight_in_transaction(root, &patch, &transaction);
        if !matches!(fresh, Ok(ref fresh) if fresh.digest()? == current.digest()?) {
            receipt.state = ValidationState::Stale;
            break;
        }
        if transaction.verify_execution_context(run_id, plan).is_err() {
            receipt.state = ValidationState::Stale;
            break;
        }
        let budget = Duration::from_secs(plan.limits.total_secs)
            .saturating_sub(started.elapsed())
            .min(Duration::from_secs(plan.limits.per_check_secs));
        if budget <= CLEANUP_RESERVE {
            receipt.state = ValidationState::Invalid;
            break;
        }
        // Progress includes only an ordinal: no command output, secrets, or
        // token identifiers can escape through the supplied sink.
        writeln!(
            progress,
            "Validation check {}/{} starting",
            receipt.checks.len() + 1,
            plan.checks.len()
        )
        .map_err(|_| invalid("validation progress sink failed"))?;
        let runner = CheckRunner::new(
            engine,
            &approver,
            Interactivity::Interactive,
            trusted,
            workspace.root(),
        )
        .with_owned_processes()
        .with_exact_commands(exact.clone())
        .with_env(EnvPolicy::Only {
            keep: plan.environment_names.clone(),
            set: plan
                .environment_set
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
        })
        .with_timeout(budget - CLEANUP_RESERVE)
        .with_cancel(cancel.clone());
        let config = CheckConfig {
            name: check.name.clone(),
            program: check.program.clone(),
            args: check.args.clone(),
            fix_program: None,
            fix_args: Vec::new(),
            cadence: Default::default(),
            auto_fix: AutoFix::No,
            severity: None,
        };
        let run = runner.execute(&config).await;
        let end = match run.end {
            CommandEnd::Exited {
                code: Some(code),
                success,
            } if success == (code == 0) => ValidationCommandEnd::Exited { code },
            CommandEnd::Denied => ValidationCommandEnd::Denied,
            CommandEnd::NotStarted(_) => ValidationCommandEnd::NotStarted,
            CommandEnd::TimedOut => ValidationCommandEnd::TimedOut,
            CommandEnd::Cancelled => ValidationCommandEnd::Cancelled,
            _ => ValidationCommandEnd::Failed,
        };
        let mut stdout = run.stdout;
        let mut stderr = run.stderr;
        let cap = plan.limits.stream_bytes as usize;
        let truncated = run.truncated | trim(&mut stdout, cap) | trim(&mut stderr, cap);
        let cleanup_confirmed = run.cleanup == CleanupOutcome::Confirmed;
        safely_stopped &=
            run.pipes_closed && (cleanup_confirmed || run.cleanup == CleanupOutcome::NotStarted);
        let passed = end == (ValidationCommandEnd::Exited { code: 0 })
            && cleanup_confirmed
            && run.pipes_closed
            && run.elapsed <= budget;
        receipt.checks.push(ValidationCheckEvidence {
            command: check.clone(),
            end: end.clone(),
            elapsed_ms: run.elapsed.as_millis().try_into().unwrap_or(u64::MAX),
            stdout,
            stderr,
            truncated,
            pipes_closed: run.pipes_closed,
            cleanup_confirmed,
        });
        receipt.finished_at = now()?;
        patch.record_validation(&transaction, receipt.clone())?;
        writeln!(
            progress,
            "Validation check {}/{} observed",
            receipt.checks.len(),
            plan.checks.len()
        )
        .map_err(|_| invalid("validation progress sink failed"))?;
        if !passed {
            receipt.state = if !safely_stopped || !run.pipes_closed || run.elapsed > budget {
                ValidationState::Invalid
            } else if end == ValidationCommandEnd::Cancelled {
                ValidationState::Cancelled
            } else {
                ValidationState::Failed
            };
            break;
        }
    }
    // Re-read complete source, host binaries and private execution context after
    // proposed code ran. An inspection failure is itself nonpassing evidence.
    match preflight_in_transaction(root, &patch, &transaction) {
        Ok(fresh) if fresh.digest()? == current.digest()? => {}
        _ => receipt.state = ValidationState::Stale,
    }
    if transaction.verify_execution_context(run_id, plan).is_err() {
        receipt.state = ValidationState::Stale;
    }
    receipt.finished_at = now()?;
    if started.elapsed() > Duration::from_secs(plan.limits.total_secs) {
        receipt.state = ValidationState::Invalid;
    }
    // The initial registered profile declares no artifacts. Any future profile
    // must implement contained, size-bounded collection before it can pass here.
    if !plan.artifact_paths.is_empty() {
        receipt.state = ValidationState::Invalid;
    }
    if receipt.state == ValidationState::Running {
        receipt.state = ValidationState::Passed;
        if receipt
            .verify_current(&current.proposal.identity, plan, now()?)
            .is_err()
        {
            receipt.state = ValidationState::Invalid;
        }
    }
    // Persist before returning success or completing the journal. A crash in
    // between remains recovery-required, including if the receipt says Passed.
    patch.record_validation(&transaction, receipt.clone())?;
    if safely_stopped {
        transaction.finish(
            run_id,
            "validation observations persisted; no promotion performed",
        )?;
    }
    Ok(receipt)
}

// Constructed only after consuming an EvaluationToken; the runner's separate
// typed exact-command restriction prevents ambiguous display strings widening
// this Ask adapter. PermissionEngine Deny always precedes this callback.
struct ConsumedPlanApprover<'a> {
    commands: &'a [ExactCommand],
    trusted: bool,
}

impl Approver for ConsumedPlanApprover<'_> {
    fn approve<'a>(
        &'a self,
        request: &'a PermissionRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        let allowed = request.tool == "quality_check"
            && request.trusted == self.trusted
            && request.interactivity == Interactivity::Interactive
            && matches!(request.effect, Effect::RunCommand(_))
            && self.commands.iter().any(|command| {
                let detail = if command.args.is_empty() {
                    command.program.clone()
                } else {
                    format!("{} {}", command.program, command.args.join(" "))
                };
                request.detail == detail
            });
        Box::pin(async move { allowed })
    }
}

fn trim(text: &mut String, cap: usize) -> bool {
    if text.len() <= cap {
        return false;
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    true
}

fn now() -> Result<u64, ValidationError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|time| time.as_secs())
        .map_err(|_| ValidationError::Clock)
}

fn invalid(message: &str) -> ValidationError {
    ValidationError::Invalid(message.into())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::proposal_preflight;
    use localpilot_patchgen::{propose, ChangeProvenance, PatchProposal, ProposedEdit};
    use localpilot_sandbox::Profile;
    use std::fs;

    fn git(root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "commit.gpgSign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().into()
    }

    fn fixture(source: &str) -> (tempfile::TempDir, ProposedPatch, String) {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-b", "main"]);
        git(root.path(), &["config", "user.name", "fixture"]);
        git(root.path(), &["config", "user.email", "fixture@localhost"]);
        git(root.path(), &["config", "core.autocrlf", "false"]);
        fs::create_dir(root.path().join("src")).unwrap();
        fs::write(
            root.path().join("src/lib.rs"),
            "pub fn value() -> u32 { 1 }\n",
        )
        .unwrap();
        fs::write(root.path().join(".gitignore"), ".localpilot/\ntarget/\n").unwrap();
        fs::write(
            root.path().join("Cargo.toml"),
            "[package]\nname='validation-fixture'\nversion='0.0.0'\nedition='2021'\n",
        )
        .unwrap();
        fs::write(
            root.path().join("Cargo.lock"),
            "version = 3\n[[package]]\nname = \"validation-fixture\"\nversion = \"0.0.0\"\n",
        )
        .unwrap();
        // Exercise exactly the toolchain running the test, including the release
        // compiler CI lane. This is fixture configuration, not product fallback.
        let selector = std::env::var("RUSTUP_TOOLCHAIN").unwrap_or_else(|_| "1.82".into());
        fs::write(
            root.path().join("rust-toolchain.toml"),
            format!("[toolchain]\nchannel={selector:?}\n"),
        )
        .unwrap();
        fs::write(
            root.path().join(".localpilot.toml"),
            "[[harness.checks]]\nname='fixture tests'\nprogram='cargo'\nargs=['test','--lib']\n",
        )
        .unwrap();
        git(root.path(), &["add", "-f", "."]);
        git(
            root.path(),
            &["commit", "-m", "controlled validation fixture"],
        );
        let head = git(root.path(), &["rev-parse", "HEAD"]);
        let patch = propose(
            root.path(),
            "validation-fixture",
            &PatchProposal::new(
                "controlled validation fixture",
                vec!["src/lib.rs".into()],
                vec![ProposedEdit::new("src/lib.rs", source)],
            ),
            ChangeProvenance::new("fixture", "scripted", "execute-only observations"),
        )
        .unwrap();
        patch.persist().unwrap();
        let patch = ProposedPatch::reopen(root.path(), "validation-fixture").unwrap();
        (root, patch, head)
    }

    async fn run(
        root: &Path,
        patch: &ProposedPatch,
        profile: Profile,
        cancel: CancelSignal,
    ) -> ValidationEvidence {
        let review = proposal_preflight(root, patch.id()).unwrap();
        let token = EvaluationToken::approve(
            &review.proposal.identity,
            &review.plan,
            "fixture owner",
            "run exactly the disposable fixture plan",
        )
        .unwrap();
        let engine = PermissionEngine::new(profile, Vec::new());
        let mut progress = Vec::new();
        let receipt = validate_proposal(
            root,
            patch.id(),
            &review,
            token,
            &engine,
            true,
            cancel,
            &mut progress,
        )
        .await
        .unwrap();
        assert!(!String::from_utf8(progress).unwrap().contains("token"));
        receipt
    }

    #[tokio::test]
    async fn explicit_cold_and_warm_grants_pass_through_ask_and_persist_before_return() {
        let (root, patch, head) = fixture("pub fn value() -> u32 { 2 }\n#[test] fn works() { assert_eq!(value(), 2); assert!(std::env::var_os(\"LOCALMIND_GLOBAL_ROOT\").is_none()); assert!(std::env::var_os(\"OPENAI_API_KEY\").is_none()); println!(\"fixture passed\"); }\n");
        let mut runs = Vec::new();
        for label in ["cold", "warm"] {
            let started = Instant::now();
            let receipt = run(root.path(), &patch, Profile::Default, CancelSignal::new()).await;
            eprintln!(
                "explicit fixture {label}: {}ms; command {}ms",
                started.elapsed().as_millis(),
                receipt.checks[0].elapsed_ms
            );
            assert_eq!(receipt.state, ValidationState::Passed, "{receipt:?}");
            let reopened = ProposedPatch::reopen(root.path(), patch.id()).unwrap();
            assert_eq!(reopened.provenance().validation.as_ref(), Some(&receipt));
            let fresh = proposal_preflight(root.path(), patch.id()).unwrap();
            receipt
                .verify_current(&fresh.proposal.identity, &fresh.plan, now().unwrap())
                .unwrap();
            assert_eq!(git(root.path(), &["rev-parse", "HEAD"]), head);
            assert!(!root.path().join("target").exists());
            runs.push(receipt.run_id);
        }
        assert_ne!(runs[0], runs[1]);
        let mut authority = RepositoryAuthority::open(root.path()).unwrap();
        let transaction = authority.try_transaction().unwrap();
        assert_eq!(transaction.events().count(), 2);
        assert!(transaction.events().all(|event| event.completed));
    }

    #[tokio::test]
    async fn failed_assertions_and_source_mutation_never_produce_a_passing_receipt() {
        for (source, expected) in [
            ("#[test] fn fails() { assert_eq!(1, 2); }\n", ValidationState::Failed),
            ("#[test] fn mutates() { std::fs::write(\"src/lib.rs\", \"// changed while testing\\n\").unwrap(); }\n", ValidationState::Stale),
        ] {
            let (root, patch, head) = fixture(source);
            let receipt = run(root.path(), &patch, Profile::Default, CancelSignal::new()).await;
            assert_eq!(receipt.state, expected, "{receipt:?}");
            assert!(receipt.checks[0].cleanup_confirmed);
            assert_eq!(git(root.path(), &["rev-parse", "HEAD"]), head);
            assert_eq!(ProposedPatch::reopen(root.path(), patch.id()).unwrap().provenance().validation.as_ref(), Some(&receipt));
        }
    }

    #[tokio::test]
    async fn execution_consent_never_lifts_read_only_deny_or_preexisting_cancellation() {
        let (root, patch, _) = fixture("#[test] fn works() {}\n");
        let denied = run(root.path(), &patch, Profile::ReadOnly, CancelSignal::new()).await;
        assert_eq!(denied.state, ValidationState::Failed);
        assert_eq!(denied.checks[0].end, ValidationCommandEnd::Denied);
        assert!(!patch.worktree_path().join("target").exists());
        let cancel = CancelSignal::new();
        cancel.cancel();
        let cancelled = run(root.path(), &patch, Profile::Default, cancel).await;
        assert_eq!(cancelled.state, ValidationState::Cancelled);
        assert_eq!(cancelled.checks[0].end, ValidationCommandEnd::Cancelled);
        assert!(!patch.worktree_path().join("target").exists());
    }

    #[test]
    fn output_cap_keeps_complete_utf8_and_exact_byte_limit() {
        let mut output = "é".repeat(100);
        assert!(trim(&mut output, 19));
        assert_eq!(output.len(), 18);
        assert!(!trim(&mut output, 19));
    }
}
