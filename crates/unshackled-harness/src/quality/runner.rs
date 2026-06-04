//! Quality-gate check execution.
//!
//! A check runs through the *same* permission engine and classification as any
//! other command (ADR-0009, docs/05): the runner builds a [`PermissionRequest`]
//! under a distinct tool identity, asks [`PermissionEngine::decide`], and spawns
//! only when allowed. There is no path that skips the decision. Output is bounded
//! and redacted before it becomes a finding.

use std::path::Path;
use std::time::Duration;

use unshackled_config::redact;
use unshackled_config::{AutoFix, CheckConfig, RuleSeverity};
use unshackled_sandbox::{
    classify, Approver, Decision, Effect, Interactivity, PermissionEngine, PermissionRequest,
};

/// The tool identity quality-gate checks present to the permission engine. A
/// distinct name (not `run_shell`) means a ratification allowlist can authorize
/// the gate without authorizing arbitrary shell.
pub const QUALITY_CHECK_TOOL: &str = "quality_check";

/// Cap on captured check output before truncation.
const MAX_OUTPUT_BYTES: usize = 16 * 1024;

/// Default per-check timeout. Full checks (test suites) can be slow.
const DEFAULT_TIMEOUT_SECS: u64 = 300;

/// What happened when a check ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    /// The check command exited successfully.
    Passed,
    /// The check ran and reported findings (non-zero exit).
    Failed,
    /// The permission engine denied, or the user declined, the command.
    Denied,
    /// The command could not be started or timed out.
    Errored,
}

/// The result of running one quality-gate check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutcome {
    /// The check's name.
    pub name: String,
    /// What happened.
    pub status: CheckStatus,
    /// Bounded, redacted detail (exit code + captured output). Empty on a clean
    /// pass.
    pub detail: String,
    /// Whether a fixer ran and the check was re-run.
    pub fixed: bool,
    /// The check's configured severity, carried so the `quality_gate` rule can
    /// apply a per-check override (e.g. an advisory `audit` blocks).
    pub severity: Option<RuleSeverity>,
}

impl CheckOutcome {
    /// Whether the check passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.status == CheckStatus::Passed
    }
}

/// The outcome of a single command invocation, before fix orchestration.
enum RunResult {
    /// Allowed and run; `success` is the exit-code verdict.
    Ran { success: bool, detail: String },
    /// The permission engine denied or the user declined.
    Denied,
    /// The command could not be started or timed out.
    Errored(String),
}

/// Runs quality-gate checks through the permission engine and the sandbox.
pub struct CheckRunner<'a> {
    engine: &'a PermissionEngine,
    approver: &'a dyn Approver,
    interactivity: Interactivity,
    trusted: bool,
    root: &'a Path,
    timeout: Duration,
}

impl<'a> CheckRunner<'a> {
    /// A runner that evaluates each command against `engine` (consulting
    /// `approver` on an `Ask`) and runs allowed commands in `root`.
    #[must_use]
    pub fn new(
        engine: &'a PermissionEngine,
        approver: &'a dyn Approver,
        interactivity: Interactivity,
        trusted: bool,
        root: &'a Path,
    ) -> Self {
        Self {
            engine,
            approver,
            interactivity,
            trusted,
            root,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        }
    }

    /// Override the per-check timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Run a check; when it fails and `auto_fix` allows it, run the fixer and
    /// re-run the check once. Every command goes through the permission engine.
    pub async fn run(&self, check: &CheckConfig) -> CheckOutcome {
        match self.run_command(&check.program, &check.args).await {
            RunResult::Ran { success: true, .. } => {
                self.outcome(check, CheckStatus::Passed, String::new(), false)
            }
            RunResult::Denied => self.outcome(
                check,
                CheckStatus::Denied,
                "permission engine denied the check command".to_string(),
                false,
            ),
            RunResult::Errored(detail) => self.outcome(check, CheckStatus::Errored, detail, false),
            RunResult::Ran {
                success: false,
                detail,
            } => self.maybe_fix(check, detail).await,
        }
    }

    /// On a failing check, run the fixer (if `auto_fix` permits one) and re-run
    /// the check once; otherwise report the failure as-is.
    async fn maybe_fix(&self, check: &CheckConfig, first_detail: String) -> CheckOutcome {
        let Some((program, args)) = fix_invocation(check) else {
            return self.outcome(check, CheckStatus::Failed, first_detail, false);
        };
        // The fixer is itself a permission-checked command; its own result does
        // not decide the outcome — the re-run of the check does.
        let _ = self.run_command(program, args).await;
        match self.run_command(&check.program, &check.args).await {
            RunResult::Ran { success: true, .. } => {
                self.outcome(check, CheckStatus::Passed, String::new(), true)
            }
            RunResult::Ran {
                success: false,
                detail,
            } => self.outcome(check, CheckStatus::Failed, detail, true),
            RunResult::Denied => self.outcome(
                check,
                CheckStatus::Denied,
                "permission engine denied the check re-run".to_string(),
                true,
            ),
            RunResult::Errored(detail) => self.outcome(check, CheckStatus::Errored, detail, true),
        }
    }

    fn outcome(
        &self,
        check: &CheckConfig,
        status: CheckStatus,
        detail: String,
        fixed: bool,
    ) -> CheckOutcome {
        CheckOutcome {
            name: check.name.clone(),
            status,
            detail,
            fixed,
            severity: check.severity,
        }
    }

    /// Classify, ask the permission engine, and — only if allowed — spawn the
    /// command in the workspace root, capturing a bounded, redacted result.
    async fn run_command(&self, program: &str, args: &[String]) -> RunResult {
        let class = classify(program, args);
        let request = PermissionRequest {
            tool: QUALITY_CHECK_TOOL,
            effect: Effect::RunCommand(class),
            interactivity: self.interactivity,
            trusted: self.trusted,
            detail: command_line(program, args),
        };
        let allowed = match self.engine.decide(&request) {
            Decision::Allow => true,
            Decision::Deny => false,
            Decision::Ask => self.approver.approve(&request).await,
        };
        if !allowed {
            return RunResult::Denied;
        }

        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .current_dir(self.root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => return RunResult::Errored(format!("failed to start {program}: {error}")),
        };
        match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => {
                let code = output.status.code().unwrap_or(-1);
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let detail = bound(redact::redact(&format!(
                    "exit: {code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
                )));
                RunResult::Ran {
                    success: output.status.success(),
                    detail,
                }
            }
            Ok(Err(error)) => RunResult::Errored(error.to_string()),
            Err(_) => {
                RunResult::Errored(format!("check timed out after {}s", self.timeout.as_secs()))
            }
        }
    }
}

/// The fixer invocation for a check, if `auto_fix` permits one and a fixer is
/// configured. `Safe` and `Full` both run the configured fixer; the distinction
/// is which command the profile chose, not how the runner invokes it.
fn fix_invocation(check: &CheckConfig) -> Option<(&str, &[String])> {
    match check.auto_fix {
        AutoFix::No => None,
        AutoFix::Safe | AutoFix::Full => check
            .fix_program
            .as_deref()
            .map(|program| (program, check.fix_args.as_slice())),
    }
}

fn command_line(program: &str, args: &[String]) -> String {
    if args.is_empty() {
        program.to_string()
    } else {
        format!("{program} {}", args.join(" "))
    }
}

/// Truncate `text` to the output cap on a char boundary.
fn bound(mut text: String) -> String {
    if text.len() <= MAX_OUTPUT_BYTES {
        return text;
    }
    let mut end = MAX_OUTPUT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("\n... [output truncated]");
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use unshackled_sandbox::{Profile, ScriptedApprover};

    fn bypass() -> PermissionEngine {
        PermissionEngine::new(Profile::Bypass, Vec::new())
    }

    fn check(
        program: &str,
        args: &[&str],
        auto_fix: AutoFix,
        fix: Option<(&str, &[&str])>,
    ) -> CheckConfig {
        CheckConfig {
            name: "t".to_string(),
            program: program.to_string(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
            fix_program: fix.map(|(p, _)| p.to_string()),
            fix_args: fix
                .map(|(_, a)| a.iter().map(|a| (*a).to_string()).collect())
                .unwrap_or_default(),
            cadence: unshackled_config::Cadence::Phase,
            auto_fix,
            severity: None,
        }
    }

    // Cross-platform command builders (no shell assumptions baked into the gate).
    #[cfg(windows)]
    fn exit_with(code: i32) -> (String, Vec<String>) {
        (
            "cmd".to_string(),
            vec!["/C".to_string(), format!("exit {code}")],
        )
    }
    #[cfg(not(windows))]
    fn exit_with(code: i32) -> (String, Vec<String>) {
        (
            "sh".to_string(),
            vec!["-c".to_string(), format!("exit {code}")],
        )
    }

    #[cfg(windows)]
    fn require_marker() -> (String, Vec<String>) {
        (
            "cmd".to_string(),
            vec!["/C".to_string(), "dir marker.txt".to_string()],
        )
    }
    #[cfg(not(windows))]
    fn require_marker() -> (String, Vec<String>) {
        ("ls".to_string(), vec!["marker.txt".to_string()])
    }

    #[cfg(windows)]
    fn create_marker() -> (String, Vec<String>) {
        (
            "cmd".to_string(),
            vec!["/C".to_string(), "type nul > marker.txt".to_string()],
        )
    }
    #[cfg(not(windows))]
    fn create_marker() -> (String, Vec<String>) {
        ("touch".to_string(), vec!["marker.txt".to_string()])
    }

    #[tokio::test]
    async fn a_passing_command_is_reported_passed() {
        let dir = tempfile::tempdir().unwrap();
        let engine = bypass();
        let approver = ScriptedApprover::always();
        let runner = CheckRunner::new(
            &engine,
            &approver,
            Interactivity::NonInteractive,
            true,
            dir.path(),
        );
        let (program, args) = exit_with(0);
        let outcome = runner
            .run(&check(&program, &refs(&args), AutoFix::No, None))
            .await;
        assert_eq!(outcome.status, CheckStatus::Passed);
        assert!(outcome.passed());
        assert!(!outcome.fixed);
    }

    #[tokio::test]
    async fn a_failing_command_is_reported_failed_with_detail() {
        let dir = tempfile::tempdir().unwrap();
        let engine = bypass();
        let approver = ScriptedApprover::always();
        let runner = CheckRunner::new(
            &engine,
            &approver,
            Interactivity::NonInteractive,
            true,
            dir.path(),
        );
        let (program, args) = exit_with(1);
        let outcome = runner
            .run(&check(&program, &refs(&args), AutoFix::No, None))
            .await;
        assert_eq!(outcome.status, CheckStatus::Failed);
        assert!(outcome.detail.contains("exit: 1"));
    }

    #[tokio::test]
    async fn a_denied_command_is_not_spawned() {
        // Default profile, non-interactive: an Unknown-class command is denied,
        // so a nonexistent program is never spawned (which would Error instead).
        let dir = tempfile::tempdir().unwrap();
        let engine = PermissionEngine::new(Profile::Default, Vec::new());
        let approver = ScriptedApprover::new(vec![false]);
        let runner = CheckRunner::new(
            &engine,
            &approver,
            Interactivity::NonInteractive,
            true,
            dir.path(),
        );
        let outcome = runner
            .run(&check(
                "definitely-not-a-real-program-xyzzy",
                &[],
                AutoFix::No,
                None,
            ))
            .await;
        assert_eq!(outcome.status, CheckStatus::Denied);
    }

    #[tokio::test]
    async fn auto_fix_runs_the_fixer_and_re_runs_to_pass() {
        let dir = tempfile::tempdir().unwrap();
        let engine = bypass();
        let approver = ScriptedApprover::always();
        let runner = CheckRunner::new(
            &engine,
            &approver,
            Interactivity::NonInteractive,
            true,
            dir.path(),
        );
        let (check_program, check_args) = require_marker();
        let (fix_program, fix_args) = create_marker();
        let cfg = check(
            &check_program,
            &refs(&check_args),
            AutoFix::Full,
            Some((&fix_program, &refs(&fix_args))),
        );
        let outcome = runner.run(&cfg).await;
        assert_eq!(outcome.status, CheckStatus::Passed);
        assert!(outcome.fixed);
        assert!(dir.path().join("marker.txt").is_file());
    }

    #[tokio::test]
    async fn no_auto_fix_means_the_fixer_never_runs() {
        let dir = tempfile::tempdir().unwrap();
        let engine = bypass();
        let approver = ScriptedApprover::always();
        let runner = CheckRunner::new(
            &engine,
            &approver,
            Interactivity::NonInteractive,
            true,
            dir.path(),
        );
        let (check_program, check_args) = require_marker();
        let (fix_program, fix_args) = create_marker();
        let cfg = check(
            &check_program,
            &refs(&check_args),
            AutoFix::No,
            Some((&fix_program, &refs(&fix_args))),
        );
        let outcome = runner.run(&cfg).await;
        assert_eq!(outcome.status, CheckStatus::Failed);
        assert!(!outcome.fixed);
        assert!(!dir.path().join("marker.txt").is_file());
    }

    fn refs(args: &[String]) -> Vec<&str> {
        args.iter().map(String::as_str).collect()
    }
}
