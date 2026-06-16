//! Tool-discipline benchmark: scenarios that measure how well the *current*
//! agent loop uses tools, scored against the resulting repository state and the
//! [`EvidenceLedger`] projection of the session event log.
//!
//! Like the golden-task evals, scenarios are authored for this repository and
//! run offline against the scripted [`FakeProvider`]; an optional live path is
//! gated behind `LOCALPILOT_LIVE_TESTS`. Scripted mode proves the mechanics
//! (the ledger sees what the loop did); live mode would score model behaviour.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use localpilot_harness::{
    resume_one_step, CallOutcome, EvidenceLedger, RuleEngine, SessionConfig, SessionRuntime,
};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use serde_json::{json, Value};

/// One discipline scenario: a repository setup, the tool calls the model emits,
/// its final claim, and a success check over **state and the evidence ledger**.
struct DisciplineTask {
    name: &'static str,
    /// Fixture files written into the scenario workspace.
    files: Vec<(&'static str, &'static str)>,
    /// The plan step the loop executes.
    step: &'static str,
    /// The tool calls the model emits, scripted for the offline runner.
    script: Vec<(&'static str, &'static str, Value)>, // (tool, id, input)
    /// The model's final assistant text.
    final_text: &'static str,
    /// Tools the scenario expects to be in play (declarative; scored in metrics).
    available_tools: &'static [&'static str],
    /// Tool names that, if called, mean the model took a trap.
    traps: &'static [&'static str],
    /// Tool-name sequences that count as disciplined for this task.
    acceptable_sequences: &'static [&'static [&'static str]],
    /// Named behaviours that count as a discipline violation.
    invalid_behaviours: &'static [&'static str],
    /// Success asserts on the resulting state and the evidence ledger.
    success: fn(&Path, &EvidenceLedger) -> bool,
}

/// The outcome of running one scenario offline.
struct DisciplineScore {
    name: &'static str,
    passed: bool,
    ledger: EvidenceLedger,
}

fn git(root: &Path, args: &[&str]) {
    assert!(Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .unwrap()
        .success());
}

/// Drive one scenario through the real loop offline and project its event log.
fn run_discipline(task: &DisciplineTask) -> DisciplineScore {
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
            "# Progress: discipline\nBranch: feature/discipline\n\n## Steps\n\n- [ ] 1. {}\n",
            task.step
        ),
    )
    .unwrap();

    git(root, &["init"]);
    git(root, &["config", "user.email", "discipline@example.com"]);
    git(root, &["config", "user.name", "Discipline"]);
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
    let session = runtime.session_id();

    let rules = RuleEngine::with_baseline(&Default::default());
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(resume_one_step(&mut runtime, root, &rules, None, &[], 3))
        .unwrap();

    let events = Store::open(root).read_events(session).unwrap();
    let ledger = EvidenceLedger::project(&events);
    let passed = (task.success)(root, &ledger);
    DisciplineScore {
        name: task.name,
        passed,
        ledger,
    }
}

/// A symbol whose definition a disciplined model verifies before claiming about
/// it. The value is irrelevant; only that a search must surface it.
const SYMBOL_FILE: &str = "src/util.rs";

fn scenarios() -> Vec<DisciplineTask> {
    vec![
        // Disciplined: the model searches for the symbol, then grounds its claim
        // in the search result.
        DisciplineTask {
            name: "search before claiming",
            files: vec![(
                SYMBOL_FILE,
                "pub fn normalize_path(p: &str) -> String { p.to_string() }\n",
            )],
            step: "State where normalize_path is defined",
            script: vec![("search_text", "c1", json!({ "query": "normalize_path" }))],
            final_text: "normalize_path is defined in src/util.rs.",
            available_tools: &["search_text"],
            traps: &[],
            acceptable_sequences: &[&["search_text"]],
            invalid_behaviours: &["claim_without_search"],
            success: |_root, ledger| {
                ledger.calls().iter().any(|call| {
                    call.name == "search_text"
                        && call.outcome == CallOutcome::Ok
                        && call.claim_referenced
                })
            },
        },
        // Negative control: the model claims about the symbol without searching;
        // the ledger has no grounding call, so the scenario must not pass.
        DisciplineTask {
            name: "negative control (claim without search)",
            files: vec![(
                SYMBOL_FILE,
                "pub fn normalize_path(p: &str) -> String { p.to_string() }\n",
            )],
            step: "State where normalize_path is defined",
            script: vec![],
            final_text: "normalize_path is defined in src/util.rs.",
            available_tools: &["search_text"],
            traps: &[],
            acceptable_sequences: &[&["search_text"]],
            invalid_behaviours: &["claim_without_search"],
            success: |_root, ledger| {
                ledger.calls().iter().any(|call| {
                    call.name == "search_text"
                        && call.outcome == CallOutcome::Ok
                        && call.claim_referenced
                })
            },
        },
    ]
}

#[test]
fn discipline_scenarios_run_green_offline() {
    let scores: Vec<DisciplineScore> = scenarios().iter().map(run_discipline).collect();

    for (task, score) in scenarios().iter().zip(&scores) {
        eprintln!(
            "discipline: {} passed={} calls={} tools={} traps={} seqs={} invalid={}",
            score.name,
            score.passed,
            score.ledger.calls().len(),
            task.available_tools.len(),
            task.traps.len(),
            task.acceptable_sequences.len(),
            task.invalid_behaviours.len(),
        );
    }

    let disciplined = scores
        .iter()
        .find(|s| s.name == "search before claiming")
        .unwrap();
    assert!(
        disciplined.passed,
        "the disciplined scenario searches then grounds its claim"
    );

    let control = scores
        .iter()
        .find(|s| s.name.starts_with("negative"))
        .unwrap();
    assert!(
        !control.passed,
        "the negative control claims without searching and must fail"
    );
}
