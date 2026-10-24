//! Quality-gate check execution.
//!
//! A check runs through the *same* permission engine and classification as any
//! other command (ADR-0009, docs/05): the runner presents each command to
//! [`PermissionEngine::decide`] under a distinct tool identity and spawns only
//! when allowed. There is no path that skips the decision — the shared
//! execution core asks the gate before every spawn, including fixers and
//! re-runs. Output is bounded and redacted before it becomes a finding.
//!
//! A command that outlives its timeout or is cancelled is reaped as a whole
//! tree with [`kill_process_tree`] — the same reap the shell tool uses — before
//! the shared core kills the child itself, so a test runner's own children do
//! not outlive it.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use localpilot_config::redact;
use localpilot_config::{AutoFix, CheckConfig, RuleSeverity};
use localpilot_sandbox::{
    classify, Approver, Decision, Effect, ExactCommand, Interactivity, PermissionEngine,
    PermissionRequest,
};
use localpilot_tools::{kill_process_tree, ProcessTreeGuard};
use localx_eval_core::check::{
    CheckCommand, CheckSpec, CommandGate, CommandProcessGuard, ManagedCommand,
};

pub use localx_eval_core::check::{
    CancelSignal, CheckOutcome, CheckSeverity, CheckStatus, CleanupOutcome, CommandEnd, CommandRun,
    EnvPolicy,
};

/// The tool identity quality-gate checks present to the permission engine. A
/// distinct name (not `run_shell`) means a ratification allowlist can authorize
/// the gate without authorizing arbitrary shell.
pub const QUALITY_CHECK_TOOL: &str = "quality_check";

/// Runs quality-gate checks through the permission engine and the sandbox.
pub struct CheckRunner<'a> {
    gate: PermissionGate<'a>,
    root: &'a Path,
    timeout: Option<Duration>,
    env: EnvPolicy,
    cancel: Option<CancelSignal>,
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
            gate: PermissionGate {
                engine,
                approver,
                interactivity,
                trusted,
                owned_processes: false,
                exact_commands: None,
            },
            root,
            timeout: None,
            env: EnvPolicy::Inherit,
            cancel: None,
        }
    }

    /// Keep the existing tools' OS process owner through acknowledged cleanup.
    /// Legacy quality callers retain their previous best-effort reap semantics.
    #[must_use]
    pub fn with_owned_processes(mut self) -> Self {
        self.gate.owned_processes = true;
        self
    }

    /// Restrict execution to these exact typed commands before the normal
    /// permission decision. This adds a restriction, never a permission grant;
    /// the caller must separately possess proposal execution consent.
    #[must_use]
    pub fn with_exact_commands(mut self, commands: Vec<ExactCommand>) -> Self {
        self.gate.exact_commands = Some(commands);
        self
    }

    /// Override the per-check timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// The environment the checks see; the host's whole environment unless set.
    #[must_use]
    pub fn with_env(mut self, env: EnvPolicy) -> Self {
        self.env = env;
        self
    }

    /// Stop a running check, and start no further one, once `cancel` fires.
    #[must_use]
    pub fn with_cancel(mut self, cancel: CancelSignal) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Run a check; when it fails and `auto_fix` allows it, run the fixer and
    /// re-run the check once. Every command goes through the permission engine.
    pub async fn run(&self, check: &CheckConfig) -> CheckOutcome {
        self.core().run(&to_spec(check)).await
    }

    /// Run only the check's own command, never its fixer, and report exactly
    /// how it ended: exit, timeout, cancellation, not started, or refused.
    /// For callers that judge the check rather than repair the tree.
    pub async fn execute(&self, check: &CheckConfig) -> CommandRun {
        self.core()
            .execute(&CheckCommand::new(
                check.program.clone(),
                check.args.clone(),
            ))
            .await
    }

    fn core(&self) -> localx_eval_core::check::CheckRunner<'_, PermissionGate<'a>> {
        let mut runner = localx_eval_core::check::CheckRunner::new(&self.gate, self.root)
            .with_env(self.env.clone());
        if let Some(timeout) = self.timeout {
            runner = runner.with_timeout(timeout);
        }
        if let Some(cancel) = &self.cancel {
            runner = runner.with_cancel(cancel.clone());
        }
        runner
    }
}

/// The permission-engine command policy: classify the command, ask the engine,
/// consult the approver on an `Ask`, and redact captured output.
struct PermissionGate<'a> {
    engine: &'a PermissionEngine,
    approver: &'a dyn Approver,
    interactivity: Interactivity,
    trusted: bool,
    owned_processes: bool,
    exact_commands: Option<Vec<ExactCommand>>,
}

impl CommandGate for PermissionGate<'_> {
    fn allow(&self, command: &CheckCommand) -> impl Future<Output = bool> {
        let class = classify(&command.program, &command.args);
        let request = PermissionRequest {
            tool: QUALITY_CHECK_TOOL.to_string(),
            effect: Effect::RunCommand(class),
            interactivity: self.interactivity,
            trusted: self.trusted,
            detail: command_line(command),
        };
        async move {
            let exact = ExactCommand {
                program: command.program.clone(),
                args: command.args.clone(),
            };
            if self
                .exact_commands
                .as_ref()
                .is_some_and(|commands| !commands.contains(&exact))
            {
                return false;
            }
            match self.engine.decide_command(&request, Some(&exact)) {
                Decision::Allow => true,
                Decision::Deny => false,
                Decision::Ask => self.approver.approve(&request).await,
            }
        }
    }

    fn spawn(&self, command: &mut tokio::process::Command) -> std::io::Result<ManagedCommand> {
        if self.owned_processes {
            let (child, tree) = ProcessTreeGuard::spawn(command)?;
            Ok(ManagedCommand::owned(
                child,
                Box::new(CheckProcessGuard(tree)),
            ))
        } else {
            command.spawn().map(ManagedCommand::unmanaged)
        }
    }

    fn sanitize(&self, text: String) -> String {
        redact::redact(&text)
    }

    fn reap(&self, pid: u32) -> impl Future<Output = ()> {
        kill_process_tree(pid)
    }
}

struct CheckProcessGuard(ProcessTreeGuard);

impl CommandProcessGuard for CheckProcessGuard {
    fn request_stop(&mut self) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(self.0.request_stop())
    }
    fn confirm_stopped(
        &mut self,
    ) -> std::pin::Pin<Box<dyn Future<Output = CleanupOutcome> + Send + '_>> {
        Box::pin(async move {
            if self.0.confirm_stopped().await {
                CleanupOutcome::Confirmed
            } else {
                CleanupOutcome::Unverified
            }
        })
    }
}

/// Map the configured check onto the shared execution spec: the fixer is
/// offered only when `auto_fix` permits one (`Safe` and `Full` both run the
/// configured fixer; the distinction is which command the profile chose, not
/// how the runner invokes it), and the severity is carried for the gating rule.
fn to_spec(check: &CheckConfig) -> CheckSpec {
    let fixer = match check.auto_fix {
        AutoFix::No => None,
        AutoFix::Safe | AutoFix::Full => check
            .fix_program
            .as_ref()
            .map(|program| CheckCommand::new(program.clone(), check.fix_args.clone())),
    };
    CheckSpec {
        name: check.name.clone(),
        command: CheckCommand::new(check.program.clone(), check.args.clone()),
        fixer,
        severity: check.severity.and_then(check_severity),
    }
}

/// Map the configured rule severity onto the outcome's severity. `Discard`
/// has no shared-check-runner counterpart and is rejected for per-check
/// severities at config load; a discard-configured *rule* leaves the check's
/// own outcome at the default (retry-shaped) severity and escalates at the
/// rule layer instead.
fn check_severity(severity: RuleSeverity) -> Option<CheckSeverity> {
    match severity {
        RuleSeverity::Off => Some(CheckSeverity::Off),
        RuleSeverity::Warn => Some(CheckSeverity::Warn),
        RuleSeverity::Block => Some(CheckSeverity::Block),
        RuleSeverity::Discard => None,
    }
}

fn command_line(command: &CheckCommand) -> String {
    if command.args.is_empty() {
        command.program.clone()
    } else {
        format!("{} {}", command.program, command.args.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_sandbox::{Profile, ScriptedApprover};

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
            cadence: localpilot_config::Cadence::Phase,
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

    #[tokio::test]
    async fn owned_checks_observe_cleanup_and_reject_commands_outside_the_exact_plan() {
        let dir = tempfile::tempdir().unwrap();
        let engine = bypass();
        let approver = ScriptedApprover::always();
        let (program, args) = exit_with(0);
        let runner = CheckRunner::new(
            &engine,
            &approver,
            Interactivity::Interactive,
            true,
            dir.path(),
        )
        .with_owned_processes()
        .with_exact_commands(vec![ExactCommand {
            program: program.clone(),
            args: args.clone(),
        }]);
        let result = runner
            .execute(&check(&program, &refs(&args), AutoFix::No, None))
            .await;
        assert!(matches!(
            result.end,
            CommandEnd::Exited { success: true, .. }
        ));
        assert_eq!(result.cleanup, CleanupOutcome::Confirmed);
        let (other, argv) = create_marker();
        let refused = runner
            .execute(&check(&other, &refs(&argv), AutoFix::No, None))
            .await;
        assert_eq!(refused.end, CommandEnd::Denied);
        assert_eq!(refused.cleanup, CleanupOutcome::NotStarted);
        assert!(!dir.path().join("marker.txt").exists());
    }

    #[tokio::test]
    async fn exact_execution_plan_and_approver_never_override_a_read_only_deny() {
        let dir = tempfile::tempdir().unwrap();
        let engine = PermissionEngine::new(Profile::ReadOnly, Vec::new());
        let approver = ScriptedApprover::always();
        let (program, args) = create_marker();
        let runner = CheckRunner::new(
            &engine,
            &approver,
            Interactivity::Interactive,
            true,
            dir.path(),
        )
        .with_owned_processes()
        .with_exact_commands(vec![ExactCommand {
            program: program.clone(),
            args: args.clone(),
        }]);
        let result = runner
            .execute(&check(&program, &refs(&args), AutoFix::No, None))
            .await;
        assert_eq!(result.end, CommandEnd::Denied);
        assert_eq!(result.cleanup, CleanupOutcome::NotStarted);
        assert!(!dir.path().join("marker.txt").exists());
    }

    #[tokio::test]
    async fn owned_timeout_is_observed_and_confirmed_without_a_missing_program_pass() {
        let dir = tempfile::tempdir().unwrap();
        let engine = bypass();
        let approver = ScriptedApprover::always();
        #[cfg(windows)]
        let command = check(
            "powershell.exe",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 60",
            ],
            AutoFix::No,
            None,
        );
        #[cfg(not(windows))]
        let command = check("sh", &["-c", "sleep 60"], AutoFix::No, None);
        let runner = CheckRunner::new(
            &engine,
            &approver,
            Interactivity::Interactive,
            true,
            dir.path(),
        )
        .with_owned_processes()
        .with_timeout(Duration::from_millis(100));
        let timed_out = runner.execute(&command).await;
        assert_eq!(timed_out.end, CommandEnd::TimedOut);
        assert!(timed_out.pid.is_some());
        assert_eq!(timed_out.cleanup, CleanupOutcome::Confirmed);
        assert!(timed_out.pipes_closed);
        let missing = runner
            .execute(&check(
                "missing-owned-fixture-program",
                &[],
                AutoFix::No,
                None,
            ))
            .await;
        assert!(matches!(missing.end, CommandEnd::NotStarted(_)));
        assert_eq!(missing.cleanup, CleanupOutcome::NotStarted);
    }

    #[tokio::test]
    async fn owned_runner_retires_descendants_after_parent_exit_cancellation_and_future_drop() {
        // Child writes a readiness marker, waits for release and would then
        // write a leak marker. Assert the effect after releasing it, rather than
        // interpreting a successful kill helper or an empty capture as proof.
        for mode in ["exit", "cancel", "drop"] {
            let dir = tempfile::tempdir().unwrap();
            #[cfg(windows)]
            let command = {
                std::fs::write(dir.path().join("descendant.ps1"), "Set-Content started ready; while (!(Test-Path released)) { Start-Sleep -Milliseconds 20 }; Set-Content leaked bad").unwrap();
                let tail = if mode == "exit" {
                    ""
                } else {
                    "; Start-Sleep -Seconds 60"
                };
                let script = format!("Start-Process powershell.exe -WindowStyle Hidden -ArgumentList '-NoProfile -NonInteractive -File ./descendant.ps1' -RedirectStandardOutput out.txt -RedirectStandardError err.txt; while (!(Test-Path started)) {{ Start-Sleep -Milliseconds 20 }}{tail}");
                check(
                    "powershell.exe",
                    &["-NoProfile", "-NonInteractive", "-Command", &script],
                    AutoFix::No,
                    None,
                )
            };
            #[cfg(not(windows))]
            let command = {
                std::fs::write(dir.path().join("descendant.sh"), "printf ready > started; while [ ! -f released ]; do sleep 0.02; done; printf bad > leaked\n").unwrap();
                let tail = if mode == "exit" { "" } else { "; wait" };
                let script = format!("sh ./descendant.sh > out.txt 2> err.txt & while [ ! -f started ]; do sleep 0.02; done{tail}");
                check("sh", &["-c", &script], AutoFix::No, None)
            };
            let engine = bypass();
            let approver = ScriptedApprover::always();
            let cancel = CancelSignal::new();
            let runner = CheckRunner::new(
                &engine,
                &approver,
                Interactivity::Interactive,
                true,
                dir.path(),
            )
            .with_owned_processes()
            .with_timeout(Duration::from_secs(60))
            .with_cancel(cancel.clone());
            let mut invocation = Box::pin(runner.execute(&command));
            if mode == "exit" {
                let result = tokio::time::timeout(Duration::from_secs(40), &mut invocation)
                    .await
                    .unwrap();
                assert!(
                    matches!(result.end, CommandEnd::Exited { success: true, .. }),
                    "{result:?}"
                );
                #[cfg(windows)]
                assert_eq!(result.cleanup, CleanupOutcome::Confirmed);
                assert!(dir.path().join("started").is_file());
            } else {
                tokio::time::timeout(Duration::from_secs(30), async {
                    loop {
                        tokio::select! {
                            result = &mut invocation => panic!("fixture ended before readiness: {result:?}"),
                            () = tokio::time::sleep(Duration::from_millis(20)) => {
                                if dir.path().join("started").is_file() { break; }
                            }
                        }
                    }
                }).await.unwrap();
                if mode == "cancel" {
                    cancel.cancel();
                    let result = invocation.as_mut().await;
                    assert_eq!(result.end, CommandEnd::Cancelled);
                    #[cfg(windows)]
                    assert_eq!(result.cleanup, CleanupOutcome::Confirmed);
                }
            }
            drop(invocation);
            // Drop is best effort, so give the detached Unix signal helper time
            // to run before releasing the controlled descendant.
            tokio::time::sleep(Duration::from_secs(1)).await;
            std::fs::write(dir.path().join("released"), "go").unwrap();
            tokio::time::sleep(Duration::from_millis(250)).await;
            assert!(
                !dir.path().join("leaked").exists(),
                "descendant escaped {mode}"
            );
        }
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

    #[test]
    fn severity_maps_one_to_one() {
        let mut cfg = check("x", &[], AutoFix::No, None);
        cfg.severity = Some(RuleSeverity::Block);
        assert_eq!(to_spec(&cfg).severity, Some(CheckSeverity::Block));
        cfg.severity = Some(RuleSeverity::Warn);
        assert_eq!(to_spec(&cfg).severity, Some(CheckSeverity::Warn));
        cfg.severity = Some(RuleSeverity::Off);
        assert_eq!(to_spec(&cfg).severity, Some(CheckSeverity::Off));
    }

    #[tokio::test]
    async fn a_direct_check_uses_the_user_grant_and_keeps_session_floors() {
        use localpilot_sandbox::AllowedCommand;
        let dir = tempfile::tempdir().unwrap();
        let (program, args) = exit_with(0);
        let entry = AllowedCommand {
            program: program.clone(),
            args_prefix: args.clone(),
        };
        let approver = ScriptedApprover::new(Vec::new());
        for (grant, trusted, incognito, expected) in [
            (false, true, false, CheckStatus::Denied),
            (true, true, false, CheckStatus::Passed),
            (true, false, false, CheckStatus::Denied),
            (true, true, true, CheckStatus::Denied),
        ] {
            let engine = PermissionEngine::new(Profile::ReadOnly, Vec::new())
                .with_incognito(incognito)
                .with_allowed_commands(if grant {
                    vec![entry.clone()]
                } else {
                    Vec::new()
                });
            let runner = CheckRunner::new(
                &engine,
                &approver,
                Interactivity::NonInteractive,
                trusted,
                dir.path(),
            );
            let result = runner
                .run(&check(&program, &refs(&args), AutoFix::No, None))
                .await;
            assert_eq!(
                result.status, expected,
                "grant={grant} trusted={trusted} incognito={incognito}"
            );
        }
    }

    fn refs(args: &[String]) -> Vec<&str> {
        args.iter().map(String::as_str).collect()
    }
}
