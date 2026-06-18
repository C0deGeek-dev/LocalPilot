//! Ablation integration: sweep the arm matrix over a task, exercise composite
//! scoring + attribution over the produced scorecards, and run an original,
//! clean-room set of adversarial tasks aimed at specific harness mitigations.
//!
//! Offline + deterministic: the scripted [`FakeProvider`] applies a fix the same
//! way regardless of which feature arm is configured, so the arms are equal here
//! and the test proves the *sweep + scoring + attribution machinery*. The real
//! per-feature deltas come from a live run (opportunistic per the
//! validation-evidence policy), where the arms diverge.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use localpilot_harness::{
    ablation_matrix, attribute, complexity_delta_in_diff, composite_score, extract_process,
    mean_std, rank, tests_added_in_diff, AblationArm, CompositeOutcome, DiffStat, EvidenceLedger,
    QualityBlock, ResultsBlock, RuleEngine, Scorecard, SessionConfig, SessionRuntime, SpeedBlock,
    SCORECARD_SCHEMA,
};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;

/// A small task the sweep drives: a base file with a bug, the fix the provider
/// applies, and a predicate that grades the produced workspace.
struct SweepTask {
    entry: &'static str,
    base: &'static str,
    fix: &'static str,
    problem: &'static str,
    expect: fn(&str) -> bool,
}

fn git(root: &Path, args: &[&str]) {
    assert!(Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .unwrap()
        .success());
}

fn git_out(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Run one task under one arm: materialize the base, drive the loop (the fake
/// provider applies the fix), grade by the task predicate, and emit the scorecard
/// tagged with the arm name. The arm's feature toggles ride along on the
/// scorecard's `arm` field — offline they do not change behaviour.
fn run_arm(task: &SweepTask, arm: &AblationArm) -> Scorecard {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join(task.entry), task.base).unwrap();
    std::fs::write(
        root.join("PROGRESS.md"),
        format!(
            "# Progress: ablation\nBranch: feature/ablation\n\n## Steps\n\n- [ ] 1. {}\n",
            task.problem
        ),
    )
    .unwrap();
    git(root, &["init"]);
    git(root, &["config", "user.email", "ablation@example.com"]);
    git(root, &["config", "user.name", "Ablation"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "initial"]);
    let base_ref = git_out(root, &["rev-parse", "HEAD"]).trim().to_string();

    let provider = FakeProvider::new()
        .tool_call(
            "fix",
            "write_file",
            serde_json::json!({ "path": task.entry, "content": task.fix }),
        )
        .text("applied the fix");
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
    let session = runtime.session_id();
    let rules = RuleEngine::with_baseline(&Default::default());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let started = std::time::Instant::now();
    let outcome = rt
        .block_on(localpilot_harness::resume_one_step(
            &mut runtime,
            root,
            &rules,
            None,
            &[],
            3,
        ))
        .unwrap();
    let wall_ms = started.elapsed().as_millis() as u64;

    let produced = std::fs::read_to_string(root.join(task.entry)).unwrap();
    let passed = outcome.committed && (task.expect)(&produced);
    let diff_text = git_out(
        root,
        &[
            "diff",
            "--no-color",
            &base_ref,
            "HEAD",
            "--",
            ".",
            ":!PROGRESS.md",
        ],
    );
    let diff = DiffStat::from_unified(&diff_text);
    let events = Store::open(root).read_events(session).unwrap();
    let ledger = EvidenceLedger::project(&events);

    Scorecard {
        schema: SCORECARD_SCHEMA,
        task: "ablation-task".to_string(),
        arm: arm.name.clone(),
        model: "fake".to_string(),
        results: ResultsBlock {
            passed,
            regression_safe: true,
            partial_credit: if passed { 1.0 } else { 0.0 },
            tests_total: 1,
            tests_passed: u32::from(passed),
        },
        quality: QualityBlock::from_signals(
            &diff,
            None,
            &outcome.gate,
            Some(complexity_delta_in_diff(&diff_text)),
            tests_added_in_diff(&diff_text),
        ),
        process: extract_process(&events, &ledger),
        speed: SpeedBlock::from_events(&events, wall_ms),
        judge: None,
    }
}

fn sum_task() -> SweepTask {
    SweepTask {
        entry: "solution.rs",
        base: "pub fn sum_to(n: u32) -> u32 { (1..n).sum() }\n",
        fix: "pub fn sum_to(n: u32) -> u32 { (1..=n).sum() }\n",
        problem: "Fix sum_to so it sums the inclusive range 1..=n.",
        expect: |produced| produced.contains("1..=n"),
    }
}

#[test]
fn ablation_sweep_composite_and_attribution() {
    let task = sum_task();
    let matrix = ablation_matrix();
    let cards: Vec<(String, Scorecard)> = matrix
        .iter()
        .map(|arm| (arm.name.clone(), run_arm(&task, arm)))
        .collect();

    // One scorecard per arm, each tagged with its arm and solving the task.
    assert_eq!(cards.len(), matrix.len());
    for (name, card) in &cards {
        assert_eq!(&card.arm, name);
        assert!(card.results.passed, "arm `{name}` solves the task offline");
    }

    // The composite gates on correctness and ranks the passers.
    let just: Vec<Scorecard> = cards.iter().map(|(_, c)| c.clone()).collect();
    let order = rank(&just);
    assert_eq!(order.len(), just.len());
    assert!(matches!(
        composite_score(&just[order[0]]),
        CompositeOutcome::Passed(_)
    ));

    // Attribution produces one row per feature (full vs each no-<feature>).
    let full = cards.iter().find(|(n, _)| n == "full").unwrap().1.clone();
    let ablated: Vec<(String, Scorecard)> = cards
        .iter()
        .filter(|(n, _)| n.starts_with("no-"))
        .map(|(n, c)| (n.clone(), c.clone()))
        .collect();
    let rows = attribute(&full, &ablated);
    assert_eq!(rows.len(), 5, "one attribution row per feature");
    // Offline the arms are identical, so every feature reads inert — the machinery
    // is exercised; a live sweep populates `moved` where features actually diverge.
    assert!(
        rows.iter().all(|r| !r.moved),
        "offline arms are equal, so attribution shows no movement"
    );

    // Variance over identical seeds is zero; the helper is ready for live spread.
    let composites: Vec<f64> = just
        .iter()
        .map(|c| match composite_score(c) {
            CompositeOutcome::Passed(s) | CompositeOutcome::Failed(s) => s,
        })
        .collect();
    let (_mean, std) = mean_std(&composites);
    assert!(
        std.abs() < f64::EPSILON,
        "identical offline arms have no spread"
    );
}

/// Original, clean-room adversarial tasks, each aimed at one harness mitigation's
/// weak point. Authored for this repository — not copied from any benchmark.
fn adversarial_tasks() -> Vec<(&'static str, SweepTask)> {
    vec![
        // Aimed at context compaction: the one detail that matters (the exact
        // return value) is buried in a long problem statement, where naive
        // compaction would drop it.
        (
            "buried-detail (compaction adversary)",
            SweepTask {
                entry: "solution.rs",
                base: "pub fn answer() -> u32 { 0 }\n",
                fix: "pub fn answer() -> u32 { 42 }\n",
                problem: "This module exposes a single function. The surrounding service \
                          has many requirements, configuration knobs, and historical notes \
                          that are not relevant here; ignore them. Somewhere in all of that \
                          context the one concrete requirement is that answer() must return \
                          exactly 42, not 0. Make it return 42.",
                expect: |produced| produced.contains("42"),
            },
        ),
        // Aimed at the pull-discovery broker: the task explicitly needs a specific
        // capability, the kind a broker might hide behind a working set.
        (
            "needs-specific-capability (broker adversary)",
            SweepTask {
                entry: "solution.rs",
                base: "pub fn flag() -> bool { false }\n",
                fix: "pub fn flag() -> bool { true }\n",
                problem: "Completing this task requires locating and using the precise \
                          capability that flips the feature flag. Set flag() to return true.",
                expect: |produced| produced.contains("true"),
            },
        ),
    ]
}

#[test]
fn adversarial_tasks_run_and_score() {
    let full = ablation_matrix()
        .into_iter()
        .find(|a| a.name == "full")
        .unwrap();
    for (label, task) in adversarial_tasks() {
        let card = run_arm(&task, &full);
        eprintln!("adversarial: {:<40} {}", label, card.to_json().unwrap());
        assert_eq!(card.schema, SCORECARD_SCHEMA);
        // The scorecard round-trips and the offline solver clears the adversary.
        let round: Scorecard = serde_json::from_str(&card.to_json().unwrap()).unwrap();
        assert_eq!(card, round);
        assert!(card.results.passed, "offline solver clears `{label}`");
        assert!(card.quality.diff_added > 0);
    }
}
