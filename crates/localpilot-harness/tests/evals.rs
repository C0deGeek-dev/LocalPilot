//! Golden-task evals: deterministic tasks that prove the agent actually completes
//! work, scored with a scorecard. Tasks are driven by the fake provider so they
//! run offline; an optional live mode is gated behind `LOCALPILOT_LIVE_TESTS`.
//!
//! Fixtures are authored for this repository — never copied from another
//! implementation.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use localpilot_config::{load, AutoFix, Cadence, CheckConfig, CliOverrides, ConfigPaths};
use localpilot_harness::{
    ratify_gate, resume_one_step, CheckStatus, ProposedCheck, RuleEngine, SessionConfig,
    SessionRuntime,
};
use localpilot_llm::{FakeProvider, ProviderRegistry};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{
    CommandClass, Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace,
};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// One golden task: a setup, the provider's scripted behaviour, the plan step,
/// and a check on the resulting repository.
struct GoldenTask {
    name: &'static str,
    files: Vec<(&'static str, &'static str)>,
    step: &'static str,
    script: Vec<(&'static str, &'static str, Value)>, // (tool, id, input)
    final_text: &'static str,
    expect: fn(&Path) -> bool,
}

/// The per-task scorecard fields.
#[derive(Debug)]
struct TaskScore {
    name: &'static str,
    success: bool,
    committed: bool,
}

#[derive(Debug)]
struct LiveTaskScore {
    name: &'static str,
    success: bool,
    stop_reason: String,
}

fn git(root: &Path, args: &[&str]) {
    assert!(Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .unwrap()
        .success());
}

fn run_task(task: &GoldenTask) -> TaskScore {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (rel, contents) in &task.files {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }
    std::fs::write(
        root.join("PROGRESS.md"),
        format!(
            "# Progress: eval\nBranch: feature/eval\n\n## Steps\n\n- [ ] 1. {}\n",
            task.step
        ),
    )
    .unwrap();

    git(root, &["init"]);
    git(root, &["config", "user.email", "eval@example.com"]);
    git(root, &["config", "user.name", "Eval"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "initial"]);

    let mut provider = FakeProvider::new();
    for (tool, id, input) in &task.script {
        provider = provider.tool_call(id, tool, input.clone());
    }
    provider = provider.text(task.final_text);

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
    let rt = tokio::runtime::Runtime::new().unwrap();
    let outcome = rt
        .block_on(resume_one_step(&mut runtime, root, &rules, None, &[], 3))
        .unwrap();

    TaskScore {
        name: task.name,
        success: outcome.committed && (task.expect)(root),
        committed: outcome.committed,
    }
}

fn tasks() -> Vec<GoldenTask> {
    vec![
        GoldenTask {
            name: "create a tiny CLI entrypoint",
            files: vec![],
            step: "Create src/main.rs with a hello world",
            script: vec![(
                "write_file",
                "c1",
                json!({ "path": "src/main.rs", "content": "fn main() { println!(\"hi\"); }\n" }),
            )],
            final_text: "created the entrypoint",
            expect: |root| root.join("src/main.rs").exists(),
        },
        GoldenTask {
            name: "fix a failing assertion",
            files: vec![("src/lib.rs", "pub fn two() -> i32 { 1 }\n")],
            step: "Fix two() to return 2",
            script: vec![(
                "edit_file",
                "c1",
                json!({ "path": "src/lib.rs", "old_text": "1", "new_text": "2" }),
            )],
            final_text: "fixed the return value",
            expect: |root| {
                std::fs::read_to_string(root.join("src/lib.rs"))
                    .unwrap()
                    .contains("-> i32 { 2 }")
            },
        },
        GoldenTask {
            name: "edit docs and code together",
            files: vec![
                ("README.md", "# Demo\n\ntodo\n"),
                ("notes.txt", "old note\n"),
            ],
            step: "Update README and notes",
            script: vec![
                (
                    "edit_file",
                    "c1",
                    json!({ "path": "README.md", "old_text": "todo", "new_text": "done" }),
                ),
                (
                    "write_file",
                    "c2",
                    json!({ "path": "notes.txt", "content": "new note\n" }),
                ),
            ],
            final_text: "updated docs and code",
            expect: |root| {
                std::fs::read_to_string(root.join("README.md"))
                    .unwrap()
                    .contains("done")
                    && std::fs::read_to_string(root.join("notes.txt"))
                        .unwrap()
                        .contains("new note")
            },
        },
        // A negative control: the model produces no change, so the step does not
        // complete the work — the scorecard must show this as a failure.
        GoldenTask {
            name: "negative control (no change)",
            files: vec![("keep.txt", "unchanged\n")],
            step: "Should have changed keep.txt but does not",
            script: vec![],
            final_text: "did nothing useful",
            expect: |root| std::fs::read_to_string(root.join("keep.txt")).unwrap() != "unchanged\n",
        },
    ]
}

fn live_tasks() -> Vec<GoldenTask> {
    tasks()
        .into_iter()
        .filter(|task| !task.name.starts_with("negative"))
        .collect()
}

#[test]
fn golden_task_scorecard() {
    let scores: Vec<TaskScore> = tasks().iter().map(run_task).collect();
    let total = scores.len();
    let passed = scores.iter().filter(|s| s.success).count();
    let rate = passed as f64 / total as f64;

    // Print the scorecard (visible with --nocapture); track the rate over time.
    eprintln!(
        "golden-task scorecard: {passed}/{total} ({:.0}%)",
        rate * 100.0
    );
    for score in &scores {
        eprintln!(
            "  {} success={} committed={}",
            score.name, score.success, score.committed
        );
    }

    // The three real tasks succeed; the negative control fails, so a regression in
    // any real task drops the rate below this expectation.
    assert_eq!(passed, 3, "expected exactly the three real tasks to pass");
    assert!(
        !scores
            .iter()
            .find(|s| s.name.starts_with("negative"))
            .unwrap()
            .success,
        "the negative control must not score as a success"
    );
}

#[test]
fn live_eval_is_gated_behind_an_env_var() {
    if std::env::var("LOCALPILOT_LIVE_TESTS").is_err() {
        eprintln!("skipping live eval: set LOCALPILOT_LIVE_TESTS to enable");
        return;
    }

    let cwd = std::env::current_dir().unwrap();
    let config = load(&ConfigPaths::standard(&cwd), &CliOverrides::default()).unwrap();
    let registry = match ProviderRegistry::from_config(&config) {
        Ok(registry) => registry,
        Err(err) => {
            eprintln!("skipping live eval: provider configuration is incomplete: {err}");
            return;
        }
    };
    let Some(provider) = registry.default_provider().cloned() else {
        eprintln!("skipping live eval: no default provider is configured");
        return;
    };
    let Some(model) = std::env::var("LOCALPILOT_LIVE_MODEL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| config.resolve_model(None))
    else {
        eprintln!(
            "skipping live eval: set provider.model, provider model env, or LOCALPILOT_LIVE_MODEL"
        );
        return;
    };

    let rt = tokio::runtime::Runtime::new().unwrap();
    let scores: Vec<LiveTaskScore> = live_tasks()
        .iter()
        .map(|task| rt.block_on(run_live_task(task, provider.clone(), model.clone())))
        .collect();
    let passed = scores.iter().filter(|score| score.success).count();
    let total = scores.len();
    eprintln!(
        "live golden-task scorecard: {passed}/{total} ({:.0}%)",
        (passed as f64 / total as f64) * 100.0
    );
    for score in &scores {
        eprintln!(
            "  {} success={} stop_reason={}",
            score.name, score.success, score.stop_reason
        );
    }
}

/// A self-fixing gate check: it passes only once `marker.txt` exists, and its
/// configured fixer creates that file. Native command per OS (no shell baked into
/// the gate). The model never touches the marker — the gate's auto-fixer does.
fn marker_check_config() -> CheckConfig {
    #[cfg(windows)]
    let (program, args, fix_program, fix_args): (&str, &[&str], &str, &[&str]) = (
        "cmd",
        &["/C", "dir marker.txt"],
        "cmd",
        &["/C", "type nul > marker.txt"],
    );
    #[cfg(not(windows))]
    let (program, args, fix_program, fix_args): (&str, &[&str], &str, &[&str]) =
        ("ls", &["marker.txt"], "touch", &["marker.txt"]);
    CheckConfig {
        name: "marker".to_string(),
        program: program.to_string(),
        args: args.iter().map(|a| (*a).to_string()).collect(),
        fix_program: Some(fix_program.to_string()),
        fix_args: fix_args.iter().map(|a| (*a).to_string()).collect(),
        cadence: Cadence::Step,
        auto_fix: AutoFix::Full,
        severity: None,
    }
}

/// Golden eval for the full gate chain: a *discovered* check is *ratified* into
/// committed config, that config is *loaded* the way the CLI loads it, the gate
/// *runs* during a step, finds a failure, *auto-fixes* it, re-runs green, and the
/// step commits. Deterministic and offline; discovery itself is unit-tested in
/// the quality module.
#[test]
fn discovered_gate_auto_fixes_and_commits() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("brief.md"),
        "# Brief: gate\n\n## Summary\n\nGate.\n",
    )
    .unwrap();
    std::fs::write(
        root.join("PROGRESS.md"),
        "# Progress: gate\nBranch: feature/gate\n\n## Steps\n\n- [ ] 1. Write out.txt\n",
    )
    .unwrap();
    std::fs::write(
        root.join(".localpilot.toml"),
        "[harness]\nmode = \"agent\"\n",
    )
    .unwrap();
    git(root, &["init"]);
    git(root, &["config", "user.email", "eval@example.com"]);
    git(root, &["config", "user.name", "Eval"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "initial"]);

    // Discovery proposes; ratification writes the check into committed config.
    let existing = std::fs::read_to_string(root.join(".localpilot.toml")).unwrap();
    let proposed = ProposedCheck {
        check: marker_check_config(),
        class: CommandClass::ReadOnly,
    };
    let ratified = ratify_gate(&existing, &[], &[proposed]);
    std::fs::write(root.join(".localpilot.toml"), &ratified.config_text).unwrap();
    git(root, &["add", ".localpilot.toml"]);
    git(root, &["commit", "-m", "ratify gate"]);

    // The committed gate is loaded exactly as the CLI loads it.
    let config = load(&ConfigPaths::standard(root), &CliOverrides::default()).unwrap();
    let checks = config.harness.resolved_checks();

    // The model writes the step's file but not the marker; the gate's fixer
    // creates the marker, the check re-runs green, and the step commits.
    let provider = FakeProvider::new()
        .tool_call(
            "c1",
            "write_file",
            json!({ "path": "out.txt", "content": "ok\n" }),
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
    let rt = tokio::runtime::Runtime::new().unwrap();
    let outcome = rt
        .block_on(resume_one_step(
            &mut runtime,
            root,
            &rules,
            None,
            &checks,
            3,
        ))
        .unwrap();

    let marker = outcome.gate.iter().find(|outcome| outcome.name == "marker");
    let auto_fixed = marker.is_some_and(|m| m.status == CheckStatus::Passed && m.fixed);

    // Scorecard: the recorded fields for this golden task.
    eprintln!(
        "gate golden eval: ratified={} committed={} marker_auto_fixed={}",
        ratified.added.len() == 1,
        outcome.committed,
        auto_fixed
    );

    assert_eq!(ratified.added.len(), 1, "the proposed check is ratified");
    assert!(outcome.committed, "{:?}", outcome.blocked_reason);
    assert!(auto_fixed, "the gate should auto-fix the finding and pass");
    assert!(
        root.join("marker.txt").exists(),
        "the fixer created the marker"
    );
    assert!(
        root.join("out.txt").exists(),
        "the step's own change landed"
    );
}

async fn run_live_task(
    task: &GoldenTask,
    provider: std::sync::Arc<dyn localpilot_llm::ModelProvider>,
    model: String,
) -> LiveTaskScore {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (rel, contents) in &task.files {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    let (events, _rx) = broadcast::channel(128);
    let cancel = CancellationToken::new();
    let mut runtime = SessionRuntime::new(
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
            model,
            ..SessionConfig::default()
        },
        Vec::new(),
    );

    let reason = runtime
        .run_turn(
            &format!(
                "Complete this task in the workspace using the available tools: {}",
                task.step
            ),
            &events,
            &cancel,
        )
        .await;
    LiveTaskScore {
        name: task.name,
        success: (task.expect)(root),
        stop_reason: format!("{reason:?}"),
    }
}
