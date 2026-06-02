//! End-to-end `harness resume` test: a small sample repo completes one step.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use serde_json::json;
use unshackled_harness::{resume_one_step, RuleEngine, SessionConfig, SessionRuntime};
use unshackled_llm::FakeProvider;
use unshackled_recovery::{RecoveryBudget, RecoveryEngine};
use unshackled_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
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
    let outcome = resume_one_step(&mut runtime, root, &rules, None, 3)
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
