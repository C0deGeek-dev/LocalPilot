//! End-to-end `harness resume` tests: a small sample repo completes a step, and
//! the act-on-findings loop retries, replans, or blocks on the quality gate.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use serde_json::json;
use unshackled_config::{AutoFix, Cadence, CheckConfig, RuleSeverity};
use unshackled_harness::{
    decide_step, resume_one_step, CheckRunner, CompletionInputs, RuleEngine, SessionConfig,
    SessionRuntime, StepAction, QUALITY_CHECK_TOOL,
};
use unshackled_llm::FakeProvider;
use unshackled_recovery::{RecoveryBudget, RecoveryEngine};
use unshackled_sandbox::{
    classify_posix, classify_windows, CommandClass, Decision, Effect, Interactivity,
    PermissionEngine, PermissionRequest, Profile, ScriptedApprover, Workspace,
};
use unshackled_store::Store;
use unshackled_tools::ToolRegistry;

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

const PROGRESS: &str =
    "# Progress: greeting\nBranch: feature/greeting\n\n## Steps\n\n- [ ] 1. Create hello.txt\n";

#[tokio::test]
async fn resume_completes_a_step_with_a_commit_and_progress_update() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("brief.md"),
        "# Brief: greeting\n\n## Summary\n\nGreet.\n",
    )
    .unwrap();
    std::fs::write(root.join("PROGRESS.md"), PROGRESS).unwrap();

    git(root, &["init"]);
    git(root, &["config", "user.email", "test@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "initial"]);

    let commits_before = commit_count(root);

    // The model writes the requested file, then confirms.
    let provider = FakeProvider::new()
        .tool_call(
            "c1",
            "write_file",
            json!({ "path": "hello.txt", "content": "hello" }),
        )
        .text("done");

    let mut runtime = SessionRuntime::new(
        Arc::new(provider),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Bypass, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(root),
        Workspace::new(root).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            interactivity: Interactivity::NonInteractive,
            trusted: true,
            ..SessionConfig::default()
        },
        Vec::new(),
    );

    let rules = RuleEngine::with_baseline(&Default::default());
    let outcome = resume_one_step(&mut runtime, root, &rules, None, &[], 3)
        .await
        .unwrap();

    assert_eq!(outcome.step_number, 1);
    assert!(outcome.committed, "{:?}", outcome.blocked_reason);

    // The file the step asked for exists.
    assert_eq!(
        std::fs::read_to_string(root.join("hello.txt")).unwrap(),
        "hello"
    );

    // PROGRESS.md marks the step complete with a commit hash.
    let progress = std::fs::read_to_string(root.join("PROGRESS.md")).unwrap();
    assert!(progress.contains("- [x] 1. Create hello.txt"));
    assert!(progress.contains("commit:"));

    // Two new commits: the step and the progress update.
    assert_eq!(commit_count(root), commits_before + 2);
}

fn commit_count(root: &Path) -> usize {
    let out = Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .current_dir(root)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

// --- act-on-findings loop ---------------------------------------------------

/// Initialize a one-step sample repo and return its root tempdir.
fn sample_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("brief.md"),
        "# Brief: gate\n\n## Summary\n\nGate.\n",
    )
    .unwrap();
    std::fs::write(root.join("PROGRESS.md"), PROGRESS).unwrap();
    git(root, &["init"]);
    git(root, &["config", "user.email", "test@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "initial"]);
    dir
}

/// A `Bypass`-engine runtime over `provider`, so the gate's commands run without
/// an interactive prompt — the permission path is exercised by other tests.
fn runtime(root: &Path, provider: Arc<FakeProvider>) -> SessionRuntime {
    SessionRuntime::new(
        provider,
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Bypass, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(root),
        Workspace::new(root).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            interactivity: Interactivity::NonInteractive,
            trusted: true,
            ..SessionConfig::default()
        },
        Vec::new(),
    )
}

fn check(name: &str, program: &str, args: &[&str], severity: Option<RuleSeverity>) -> CheckConfig {
    CheckConfig {
        name: name.to_string(),
        program: program.to_string(),
        args: args.iter().map(|a| (*a).to_string()).collect(),
        fix_program: None,
        fix_args: Vec::new(),
        cadence: Cadence::Step,
        auto_fix: AutoFix::No,
        severity,
    }
}

/// A step-cadence check that passes only once `marker.txt` exists (no auto-fix,
/// so the model itself must create it). Native command per OS.
fn marker_check() -> CheckConfig {
    #[cfg(windows)]
    {
        check("marker", "cmd", &["/C", "dir marker.txt"], None)
    }
    #[cfg(not(windows))]
    {
        check("marker", "ls", &["marker.txt"], None)
    }
}

/// A step-cadence check that always fails, at the given severity. Native per OS.
fn failing_check(name: &str, severity: Option<RuleSeverity>) -> CheckConfig {
    #[cfg(windows)]
    {
        check(name, "cmd", &["/C", "exit 1"], severity)
    }
    #[cfg(not(windows))]
    {
        check(name, "sh", &["-c", "exit 1"], severity)
    }
}

fn write_marker_call() -> serde_json::Value {
    json!({ "path": "marker.txt", "content": "x" })
}

#[tokio::test]
async fn an_actionable_finding_retries_then_passes_within_the_limit() {
    let dir = sample_repo();
    let root = dir.path();

    // Attempt 1 makes no change (the marker check fails); attempt 2 writes the
    // marker (the check passes). `text` closes each turn so it is not an empty
    // turn, and follows the tool call so the second turn completes cleanly.
    let provider = Arc::new(
        FakeProvider::new()
            .text("done")
            .tool_call("c1", "write_file", write_marker_call())
            .text("fixed"),
    );
    let mut rt = runtime(root, Arc::clone(&provider));

    let rules = RuleEngine::with_baseline(&Default::default());
    let outcome = resume_one_step(&mut rt, root, &rules, None, &[marker_check()], 3)
        .await
        .unwrap();

    assert!(outcome.committed, "{:?}", outcome.blocked_reason);
    assert!(root.join("marker.txt").is_file());
    // Two attempts were made (the model was re-prompted after the first).
    assert!(provider.requests().len() >= 2);
}

#[tokio::test]
async fn exhausted_retries_replan_and_record_a_decision() {
    let dir = sample_repo();
    let root = dir.path();

    // The model never creates the marker, so every attempt's check fails. Each
    // turn emits non-empty text so it completes rather than tripping recovery.
    let provider = Arc::new(FakeProvider::new().text("a").text("b").text("c"));
    let mut rt = runtime(root, Arc::clone(&provider));

    let rules = RuleEngine::with_baseline(&Default::default());
    // Two attempts, then a replan.
    let outcome = resume_one_step(&mut rt, root, &rules, None, &[marker_check()], 2)
        .await
        .unwrap();

    assert!(!outcome.committed);
    assert!(outcome
        .blocked_reason
        .as_deref()
        .unwrap_or_default()
        .contains("replanned"));

    let decisions = std::fs::read_to_string(root.join("DECISIONS.md")).unwrap();
    assert!(decisions.contains("# Decisions: greeting"));
    assert!(decisions.contains("Replan step 1"));
    assert!(decisions.contains("refs: step 1"));
}

#[tokio::test]
async fn an_audit_finding_blocks_without_retrying() {
    let dir = sample_repo();
    let root = dir.path();

    let provider = Arc::new(FakeProvider::new().text("done"));
    let mut rt = runtime(root, Arc::clone(&provider));

    let rules = RuleEngine::with_baseline(&Default::default());
    let audit = failing_check("audit", Some(RuleSeverity::Block));
    let outcome = resume_one_step(&mut rt, root, &rules, None, &[audit], 3)
        .await
        .unwrap();

    assert!(!outcome.committed);
    assert!(outcome
        .blocked_reason
        .as_deref()
        .unwrap_or_default()
        .contains("audit"));
    // A blocking finding is not retried: only the single attempt ran.
    assert_eq!(provider.requests().len(), 1);
    assert!(!root.join("DECISIONS.md").exists());
}

#[tokio::test]
async fn act_on_findings_is_cross_platform() {
    // Classification is asserted directly against each per-OS classifier (a POSIX
    // command fed to the Windows classifier would misclassify, per ADR-0007), so
    // both styles are covered regardless of the host.
    let posix = ["-c".to_string(), "exit 1".to_string()];
    let windows = ["/C".to_string(), "exit 1".to_string()];
    assert_eq!(classify_posix("sh", &posix), CommandClass::Unknown);
    assert_eq!(classify_windows("cmd", &windows), CommandClass::Unknown);

    // The native failing check actually runs and yields an actionable retry.
    let dir = tempfile::tempdir().unwrap();
    let engine = PermissionEngine::new(Profile::Bypass, Vec::new());
    let approver = ScriptedApprover::always();
    let runner = CheckRunner::new(
        &engine,
        &approver,
        Interactivity::NonInteractive,
        true,
        dir.path(),
    );
    let outcome = runner.run(&failing_check("lint", None)).await;

    let rules = RuleEngine::with_baseline(&Default::default());
    let inputs = CompletionInputs {
        tests_passed: None,
        progress_reflects_completion: true,
        commit_message: "harness: step 1".to_string(),
        attempts: 1,
        max_attempts: 3,
    };
    assert!(matches!(
        decide_step(&rules, &inputs, vec![outcome]),
        StepAction::Retry(_)
    ));
}

#[test]
fn ratification_allowance_lets_the_gate_run_headless_but_grants_nothing_else() {
    // Ratifying the gate (ADR-0009) grants its tool identity a relaxed allowance,
    // so a project-write check (e.g. `cargo fmt`) runs non-interactively. The
    // allowance is keyed to the gate identity, so it never authorizes arbitrary
    // shell.
    let request = |tool: &'static str| PermissionRequest {
        tool,
        effect: Effect::RunCommand(CommandClass::ProjectWrite),
        interactivity: Interactivity::NonInteractive,
        trusted: true,
        detail: String::new(),
    };

    let with_allowance =
        PermissionEngine::new(Profile::Relaxed, vec![QUALITY_CHECK_TOOL.to_string()]);
    assert_eq!(
        with_allowance.decide(&request(QUALITY_CHECK_TOOL)),
        Decision::Allow
    );
    // The allowance does not leak to a general shell tool identity.
    assert_eq!(with_allowance.decide(&request("run_shell")), Decision::Deny);

    // Without ratification there is no allowance: the same check is denied.
    let without = PermissionEngine::new(Profile::Relaxed, Vec::new());
    assert_eq!(without.decide(&request(QUALITY_CHECK_TOOL)), Decision::Deny);
}

#[tokio::test]
async fn an_unratified_check_never_runs() {
    // Security boundary: `resume_one_step` runs only the checks it is handed (the
    // ratified gate). Discovery's proposal is never executed — passing an empty
    // gate means no check runs, even in a repo a profile would propose checks for.
    let dir = sample_repo();
    let root = dir.path();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();

    let provider = Arc::new(
        FakeProvider::new()
            .tool_call(
                "c1",
                "write_file",
                json!({ "path": "x.txt", "content": "x" }),
            )
            .text("done"),
    );
    let mut rt = runtime(root, Arc::clone(&provider));

    let rules = RuleEngine::with_baseline(&Default::default());
    let outcome = resume_one_step(&mut rt, root, &rules, None, &[], 3)
        .await
        .unwrap();

    assert!(outcome.committed, "{:?}", outcome.blocked_reason);
    assert!(outcome.gate.is_empty(), "no unratified check should run");
}
