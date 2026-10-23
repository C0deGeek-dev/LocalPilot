//! Compose the existing execute-only runner with proposal-owned durable consent.
//! An abandoned future leaves an unfinished journal entry, never reusable consent.

use crate::preflight::{preflight_in_transaction, PreflightReport};
use localpilot_config::{AutoFix, CheckConfig};
use localpilot_harness::{
    CancelSignal, CheckRunner, CleanupOutcome, CommandEnd, CommandRun, EnvPolicy,
};
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
    validate_with_elapsed(
        root,
        id,
        review,
        token,
        engine,
        trusted,
        cancel,
        progress,
        move || started.elapsed(),
    )
    .await
}

// The production clock starts before any preparation. Keeping its elapsed
// measurement injectable internally lets the fixture exercise total-budget
// exhaustion through the real coordinator without a fifteen-minute sleep.
#[allow(clippy::too_many_arguments)]
async fn validate_with_elapsed(
    root: &Path,
    id: &str,
    review: &PreflightReport,
    token: EvaluationToken,
    engine: &PermissionEngine,
    trusted: bool,
    cancel: CancelSignal,
    progress: &mut dyn Write,
    elapsed: impl Fn() -> Duration,
) -> Result<ValidationEvidence, ValidationError> {
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
            .saturating_sub(elapsed())
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
        let failure_state = check_failure(&run, budget);
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
        if let Some(state) = failure_state {
            receipt.state = state;
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
    if elapsed() > Duration::from_secs(plan.limits.total_secs) {
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
    writeln!(progress, "Validation receipt persisted")
        .map_err(|_| invalid("validation progress sink failed"))?;
    if safely_stopped {
        transaction.finish(
            run_id,
            "validation observations persisted; no promotion performed",
        )?;
    }
    Ok(receipt)
}

fn check_failure(run: &CommandRun, budget: Duration) -> Option<ValidationState> {
    if !run.pipes_closed
        || run.elapsed > budget
        || !matches!(
            run.cleanup,
            CleanupOutcome::Confirmed | CleanupOutcome::NotStarted
        )
    {
        return Some(ValidationState::Invalid);
    }
    match run.end {
        CommandEnd::Exited {
            code: Some(0),
            success: true,
        } if run.cleanup == CleanupOutcome::Confirmed => None,
        CommandEnd::Cancelled => Some(ValidationState::Cancelled),
        _ => Some(ValidationState::Failed),
    }
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
        let (root, patch, head) = fixture("pub fn value() -> u32 { 2 }\n#[test] fn works() { assert_eq!(value(), 2); assert!(std::env::var_os(\"LOCALMIND_GLOBAL_ROOT\").is_none()); assert!(std::env::var_os(\"OPENAI_API_KEY\").is_none()); use std::io::Write; let mut out = std::io::stdout(); out.write_all(b\"sk-abcdefghijklmnopqrstuvwxyz0123\\n\").unwrap(); out.write_all(&vec![b'x'; 50000]).unwrap(); }\n");
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
            assert!(receipt.checks[0].truncated);
            assert!(receipt.checks[0].stdout.len() <= 16 * 1024);
            assert!(!receipt.checks[0]
                .stdout
                .contains("sk-abcdefghijklmnopqrstuvwxyz0123"));
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

    struct CrashAtProgress {
        boundary: String,
    }

    impl Write for CrashAtProgress {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            // write_fmt can split a single line across several writes. Both
            // distinctive words are literals, independent of ordinal formatting.
            let text = String::from_utf8_lossy(bytes);
            if (self.boundary == "before_spawn" && text.contains("starting"))
                || (self.boundary == "after_observation" && text.contains("observed"))
                || (self.boundary == "after_final_receipt" && text.contains("receipt persisted"))
            {
                // An actual process exit skips every Rust destructor, unlike a
                // panic or dropping the coordinator future inside this process.
                std::process::exit(73);
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    #[ignore = "subprocess helper invoked by the actual crash boundary fixture"]
    fn crash_worker_entry() {
        let Some(root) = std::env::var_os("LOCALPILOT_VALIDATION_CRASH_ROOT") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        let review = proposal_preflight(&root, "validation-fixture").unwrap();
        let token = EvaluationToken::approve(
            &review.proposal.identity,
            &review.plan,
            "fixture owner",
            "controlled subprocess crash boundary",
        )
        .unwrap();
        let mut output = CrashAtProgress {
            boundary: std::env::var("LOCALPILOT_VALIDATION_CRASH_BOUNDARY").unwrap(),
        };
        let engine = PermissionEngine::new(Profile::Default, Vec::new());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime
            .block_on(validate_proposal(
                &root,
                "validation-fixture",
                &review,
                token,
                &engine,
                true,
                CancelSignal::new(),
                &mut output,
            ))
            .unwrap();
        panic!("requested crash boundary was not reached");
    }

    async fn assert_crash_boundary(boundary: &str) {
        let (root, patch, head) = fixture("#[test] fn works() {}\n");
        let output = tokio::time::timeout(
            Duration::from_secs(180),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "execution::tests::crash_worker_entry",
                    "--ignored",
                    "--nocapture",
                ])
                .env("LOCALPILOT_VALIDATION_CRASH_ROOT", root.path())
                .env("LOCALPILOT_VALIDATION_CRASH_BOUNDARY", boundary)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            output.status.code(),
            Some(73),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let reopened = ProposedPatch::reopen(root.path(), patch.id()).unwrap();
        let receipt = reopened.provenance().validation.as_ref().unwrap();
        let expected = if boundary == "after_final_receipt" {
            ValidationState::Passed
        } else {
            ValidationState::Running
        };
        assert_eq!(receipt.state, expected);
        if expected == ValidationState::Running {
            assert_eq!(receipt.resumed_state(), ValidationState::Invalid);
        }
        assert_eq!(
            receipt.checks.len(),
            usize::from(boundary != "before_spawn")
        );
        if boundary == "before_spawn" {
            assert!(!patch.worktree_path().join("target").exists());
        }
        let review = proposal_preflight(root.path(), patch.id()).unwrap();
        let fresh = EvaluationToken::approve(
            &review.proposal.identity,
            &review.plan,
            "fixture owner",
            "explicit fresh execution request",
        )
        .unwrap();
        let engine = PermissionEngine::new(Profile::Default, Vec::new());
        let result = validate_proposal(
            root.path(),
            patch.id(),
            &review,
            fresh,
            &engine,
            true,
            CancelSignal::new(),
            &mut Vec::new(),
        )
        .await;
        assert!(matches!(result, Err(ValidationError::RecoveryRequired)));
        let mut authority = RepositoryAuthority::open(root.path()).unwrap();
        let mut transaction = authority.try_transaction().unwrap();
        assert_eq!(transaction.events().count(), 1);
        assert!(transaction.events().all(|event| !event.completed));
        if receipt.state == ValidationState::Passed {
            // Even complete on-disk data does not replace the unfinished
            // local authority journal at the separate promotion boundary.
            let promotion = localpilot_patchgen::validation::PatchApprovalToken::approve(
                &review.proposal.identity,
                &review.plan,
                receipt,
                "fixture owner",
                "controlled promotion refusal",
            )
            .unwrap();
            assert!(transaction
                .reserve_promotion(promotion, &review.proposal.identity, &review.plan, receipt)
                .is_err());
        }
        assert_eq!(git(root.path(), &["rev-parse", "HEAD"]), head);
    }

    #[tokio::test]
    async fn crash_before_spawn_requires_recovery_and_starts_no_check() {
        assert_crash_boundary("before_spawn").await;
    }

    #[tokio::test]
    async fn crash_after_check_observation_requires_recovery() {
        assert_crash_boundary("after_observation").await;
    }

    #[tokio::test]
    async fn crash_after_passing_receipt_blocks_execution_and_promotion() {
        assert_crash_boundary("after_final_receipt").await;
    }

    struct ConcurrentAttempt {
        start: Option<std::sync::mpsc::Sender<()>>,
        observed: std::sync::mpsc::Receiver<()>,
    }

    impl Write for ConcurrentAttempt {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if let Some(start) = self.start.take() {
                start.send(()).unwrap();
                self.observed.recv_timeout(Duration::from_secs(30)).unwrap();
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn concurrent_validate_cannot_consume_a_second_grant_or_start_a_second_run() {
        let (root, patch, head) = fixture("#[test] fn works() {}\n");
        let review = proposal_preflight(root.path(), patch.id()).unwrap();
        let token = EvaluationToken::approve(
            &review.proposal.identity,
            &review.plan,
            "fixture owner",
            "first controlled run",
        )
        .unwrap();
        let second_token = EvaluationToken::approve(
            &review.proposal.identity,
            &review.plan,
            "fixture owner",
            "second controlled run",
        )
        .unwrap();
        let second_review = review.clone();
        let second_root = root.path().to_path_buf();
        let (start_tx, start_rx) = std::sync::mpsc::channel();
        let (observed_tx, observed_rx) = std::sync::mpsc::channel();
        let attempt = std::thread::spawn(move || {
            start_rx.recv_timeout(Duration::from_secs(180)).unwrap();
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let engine = PermissionEngine::new(Profile::Default, Vec::new());
            let result = runtime.block_on(validate_proposal(
                &second_root,
                "validation-fixture",
                &second_review,
                second_token,
                &engine,
                true,
                CancelSignal::new(),
                &mut Vec::new(),
            ));
            assert!(matches!(result, Err(ValidationError::Busy)), "{result:?}");
            observed_tx.send(()).unwrap();
        });
        let engine = PermissionEngine::new(Profile::Default, Vec::new());
        let mut progress = ConcurrentAttempt {
            start: Some(start_tx),
            observed: observed_rx,
        };
        let receipt = validate_proposal(
            root.path(),
            patch.id(),
            &review,
            token,
            &engine,
            true,
            CancelSignal::new(),
            &mut progress,
        )
        .await
        .unwrap();
        attempt.join().unwrap();
        assert_eq!(receipt.state, ValidationState::Passed, "{receipt:?}");
        let mut authority = RepositoryAuthority::open(root.path()).unwrap();
        let transaction = authority.try_transaction().unwrap();
        assert_eq!(transaction.events().count(), 1);
        assert!(transaction.events().all(|event| event.completed));
        assert_eq!(git(root.path(), &["rev-parse", "HEAD"]), head);
    }

    #[tokio::test]
    async fn preparation_that_exhausts_the_total_budget_starts_no_command_and_persists_invalid() {
        let (root, patch, head) = fixture("#[test] fn works() {}\n");
        let review = proposal_preflight(root.path(), patch.id()).unwrap();
        let token = EvaluationToken::approve(
            &review.proposal.identity,
            &review.plan,
            "fixture owner",
            "controlled deadline fixture",
        )
        .unwrap();
        let engine = PermissionEngine::new(Profile::Default, Vec::new());
        let receipt = validate_with_elapsed(
            root.path(),
            patch.id(),
            &review,
            token,
            &engine,
            true,
            CancelSignal::new(),
            &mut Vec::new(),
            || Duration::from_secs(901),
        )
        .await
        .unwrap();
        assert_eq!(receipt.state, ValidationState::Invalid);
        assert!(receipt.checks.is_empty());
        assert!(!patch.worktree_path().join("target").exists());
        assert_eq!(
            ProposedPatch::reopen(root.path(), patch.id())
                .unwrap()
                .provenance()
                .validation
                .as_ref(),
            Some(&receipt)
        );
        assert_eq!(git(root.path(), &["rev-parse", "HEAD"]), head);
    }

    #[tokio::test]
    async fn actual_exit_zero_without_owned_cleanup_is_invalid_at_the_receipt_boundary() {
        let root = tempfile::tempdir().unwrap();
        let engine = PermissionEngine::new(Profile::Bypass, Vec::new());
        let approver = localpilot_sandbox::ScriptedApprover::always();
        #[cfg(windows)]
        let (program, args) = ("cmd.exe", vec!["/C".into(), "exit 0".into()]);
        #[cfg(not(windows))]
        let (program, args) = ("sh", vec!["-c".into(), "exit 0".into()]);
        let command = CheckConfig {
            name: "cleanup boundary fixture".into(),
            program: program.into(),
            args,
            fix_program: None,
            fix_args: Vec::new(),
            cadence: Default::default(),
            auto_fix: AutoFix::No,
            severity: None,
        };
        let runner = CheckRunner::new(
            &engine,
            &approver,
            Interactivity::Interactive,
            true,
            root.path(),
        );
        let unowned = runner.execute(&command).await;
        assert!(matches!(
            unowned.end,
            CommandEnd::Exited { success: true, .. }
        ));
        assert!(unowned.pipes_closed);
        assert_eq!(unowned.cleanup, CleanupOutcome::Unverified);
        assert_eq!(
            check_failure(&unowned, Duration::from_secs(30)),
            Some(ValidationState::Invalid)
        );
        let owned = runner.with_owned_processes().execute(&command).await;
        assert_eq!(check_failure(&owned, Duration::from_secs(30)), None);
        // The same actual successful observation cannot pass after its approved
        // deadline, even if exit, pipes and process cleanup were all successful.
        assert_eq!(
            check_failure(&owned, Duration::ZERO),
            Some(ValidationState::Invalid)
        );
    }
}
