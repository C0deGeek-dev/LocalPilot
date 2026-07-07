//! Quality-gate check execution.
//!
//! A check runs through the *same* permission engine and classification as any
//! other command (ADR-0009, docs/05): the runner presents each command to
//! [`PermissionEngine::decide`] under a distinct tool identity and spawns only
//! when allowed. There is no path that skips the decision — the shared
//! execution core asks the gate before every spawn, including fixers and
//! re-runs. Output is bounded and redacted before it becomes a finding.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use localpilot_config::redact;
use localpilot_config::{AutoFix, CheckConfig, RuleSeverity};
use localpilot_sandbox::{
    classify, Approver, Decision, Effect, Interactivity, PermissionEngine, PermissionRequest,
};
use localx_eval_core::check::{CheckCommand, CheckSpec, CommandGate};

pub use localx_eval_core::check::{CheckOutcome, CheckSeverity, CheckStatus};

/// The tool identity quality-gate checks present to the permission engine. A
/// distinct name (not `run_shell`) means a ratification allowlist can authorize
/// the gate without authorizing arbitrary shell.
pub const QUALITY_CHECK_TOOL: &str = "quality_check";

/// Runs quality-gate checks through the permission engine and the sandbox.
pub struct CheckRunner<'a> {
    gate: PermissionGate<'a>,
    root: &'a Path,
    timeout: Option<Duration>,
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
            },
            root,
            timeout: None,
        }
    }

    /// Override the per-check timeout.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Run a check; when it fails and `auto_fix` allows it, run the fixer and
    /// re-run the check once. Every command goes through the permission engine.
    pub async fn run(&self, check: &CheckConfig) -> CheckOutcome {
        let mut runner = localx_eval_core::check::CheckRunner::new(&self.gate, self.root);
        if let Some(timeout) = self.timeout {
            runner = runner.with_timeout(timeout);
        }
        runner.run(&to_spec(check)).await
    }
}

/// The permission-engine command policy: classify the command, ask the engine,
/// consult the approver on an `Ask`, and redact captured output.
struct PermissionGate<'a> {
    engine: &'a PermissionEngine,
    approver: &'a dyn Approver,
    interactivity: Interactivity,
    trusted: bool,
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
            match self.engine.decide(&request) {
                Decision::Allow => true,
                Decision::Deny => false,
                Decision::Ask => self.approver.approve(&request).await,
            }
        }
    }

    fn sanitize(&self, text: String) -> String {
        redact::redact(&text)
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

    fn refs(args: &[String]) -> Vec<&str> {
        args.iter().map(String::as_str).collect()
    }
}
