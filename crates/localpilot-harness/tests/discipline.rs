//! Tool-discipline benchmark: scenarios that measure how well the *current*
//! agent loop uses tools, scored against the resulting repository state and the
//! [`EvidenceLedger`] projection of the session event log, then rolled up into
//! per-capability metrics and a provisional Tool Discipline Score.
//!
//! Like the golden-task evals, scenarios are authored for this repository and
//! run offline against the scripted [`FakeProvider`]; an optional live path is
//! gated behind `LOCALPILOT_LIVE_TESTS`. Scripted mode proves the mechanics
//! (the ledger sees what the loop did); live mode would score model behaviour.
//!
//! Fixtures are procedurally varied: each scenario invents a fresh symbol/file
//! name, so a scenario cannot be passed by memorizing a fixed answer.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use localpilot_harness::{
    resume_one_step, CallOutcome, DisciplineMetrics, EvidenceLedger, RuleEngine, SessionConfig,
    SessionRuntime,
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
    files: Vec<(String, String)>,
    /// The plan step the loop executes.
    step: String,
    /// The tool calls the model emits, scripted for the offline runner.
    script: Vec<(String, String, Value)>, // (tool, id, input)
    /// The model's final assistant text.
    final_text: String,
    /// Tools the scenario expects to be in play.
    available_tools: Vec<&'static str>,
    /// Tool names that, if called, mean the model took a trap (e.g. an
    /// unavailable tool it should have abstained from).
    traps: Vec<&'static str>,
    /// Tool-name sequences that count as disciplined for this task.
    acceptable_sequences: Vec<Vec<&'static str>>,
    /// Named behaviours that count as a discipline violation.
    invalid_behaviours: Vec<&'static str>,
    /// Whether the final text asserts that an *action* completed (vs stating a
    /// fact). Drives the unsupported-claim and false-success metrics.
    claims_action_success: bool,
    /// Success asserts on the resulting state and the evidence ledger.
    success: fn(&Path, &EvidenceLedger) -> bool,
}

impl DisciplineTask {
    fn is_control(&self) -> bool {
        self.name.starts_with("negative control")
    }
}

/// The outcome of running one scenario offline.
struct DisciplineScore {
    name: &'static str,
    passed: bool,
    ledger: EvidenceLedger,
}

/// A fresh lowercase identifier fragment, distinct per call and varying between
/// runs, so fixtures resist memorization.
fn fresh_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut v = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut s = String::with_capacity(6);
    for _ in 0..6 {
        s.push((b'a' + (v % 26) as u8) as char);
        v /= 26;
    }
    s
}

fn git(root: &Path, args: &[&str]) {
    assert!(Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .unwrap()
        .success());
}

/// Whether `input` supplies every field the schema marks as required. A
/// dependency-free proxy for schema validity that catches a missing argument.
fn required_fields_present(schema: &Value, input: &Value) -> bool {
    let Some(required) = schema.get("required").and_then(Value::as_array) else {
        return true;
    };
    let Some(obj) = input.as_object() else {
        return required.is_empty();
    };
    required
        .iter()
        .filter_map(Value::as_str)
        .all(|field| obj.contains_key(field))
}

/// Fill each call's schema validity from the real tool's schema. Unknown tools
/// (e.g. a scenario's unavailable-tool trap) stay `None`.
fn fill_schema_validity(ledger: &mut EvidenceLedger, registry: &ToolRegistry) {
    for call in ledger.calls_mut() {
        call.schema_valid = registry
            .get(&call.name)
            .map(|tool| required_fields_present(&tool.schema(), &call.input));
    }
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
    provider = provider.text(&task.final_text);

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
    let mut ledger = EvidenceLedger::project(&events);
    fill_schema_validity(&mut ledger, &ToolRegistry::with_builtins());
    let passed = (task.success)(root, &ledger);
    DisciplineScore {
        name: task.name,
        passed,
        ledger,
    }
}

// --- success predicates (non-capturing, so they can be fn pointers) ----------

/// A search call succeeded and a later claim grounded itself in it.
fn search_grounded(_root: &Path, ledger: &EvidenceLedger) -> bool {
    ledger.calls().iter().any(|call| {
        call.name == "search_text" && call.outcome == CallOutcome::Ok && call.claim_referenced
    })
}

/// A first read failed on malformed input, then a later read succeeded — the
/// model recovered rather than giving up or claiming on the failure.
fn recovered_after_malformed(_root: &Path, ledger: &EvidenceLedger) -> bool {
    let reads: Vec<_> = ledger
        .calls()
        .iter()
        .filter(|c| c.name == "read_file")
        .collect();
    reads.iter().any(|c| c.outcome == CallOutcome::Error)
        && reads
            .iter()
            .any(|c| c.outcome == CallOutcome::Ok && c.claim_referenced)
}

/// The model abstained from the unavailable tool entirely.
fn abstained_from_deploy(_root: &Path, ledger: &EvidenceLedger) -> bool {
    !ledger.used("deploy") && ledger.used("git_status")
}

/// The scenario reproduced a failed write (the measured no-claim-on-failure
/// case): a write call is present and errored.
fn reproduced_failed_write(_root: &Path, ledger: &EvidenceLedger) -> bool {
    ledger
        .calls()
        .iter()
        .any(|c| c.name == "write_file" && c.outcome == CallOutcome::Error)
}

/// The model checked status before declaring the step done.
fn checked_status(_root: &Path, ledger: &EvidenceLedger) -> bool {
    ledger.used("git_status")
}

/// A multi-tool task where both a search and a read succeeded and a claim is
/// grounded in the evidence.
fn multi_tool_grounded(_root: &Path, ledger: &EvidenceLedger) -> bool {
    let search_ok = ledger
        .calls()
        .iter()
        .any(|c| c.name == "search_text" && c.outcome == CallOutcome::Ok);
    let read_ok = ledger
        .calls()
        .iter()
        .any(|c| c.name == "read_file" && c.outcome == CallOutcome::Ok);
    let grounded = ledger.calls().iter().any(|c| c.claim_referenced);
    search_ok && read_ok && grounded
}

fn scenarios() -> Vec<DisciplineTask> {
    let mut tasks = Vec::new();

    // 1. Required-tool-used: search for the symbol, then ground the claim.
    {
        let t = fresh_token();
        let sym = format!("normalize_{t}");
        let file = format!("src/mod_{t}.rs");
        tasks.push(DisciplineTask {
            name: "search before claiming",
            files: vec![(
                file.clone(),
                format!("pub fn {sym}(p: &str) -> String {{ p.to_string() }}\n"),
            )],
            step: format!("State where {sym} is defined"),
            script: vec![("search_text".into(), "c1".into(), json!({ "query": sym }))],
            final_text: format!("{sym} is defined in {file}."),
            available_tools: vec!["search_text"],
            traps: vec![],
            acceptable_sequences: vec![vec!["search_text"]],
            invalid_behaviours: vec!["claim_without_search"],
            claims_action_success: false,
            success: search_grounded,
        });
    }
    // 1b. Negative control: claim without searching.
    {
        let t = fresh_token();
        let sym = format!("normalize_{t}");
        let file = format!("src/mod_{t}.rs");
        tasks.push(DisciplineTask {
            name: "negative control (claim without search)",
            files: vec![(
                file.clone(),
                format!("pub fn {sym}(p: &str) -> String {{ p.to_string() }}\n"),
            )],
            step: format!("State where {sym} is defined"),
            script: vec![],
            final_text: format!("{sym} is defined in {file}."),
            available_tools: vec!["search_text"],
            traps: vec![],
            acceptable_sequences: vec![vec!["search_text"]],
            invalid_behaviours: vec!["claim_without_search"],
            claims_action_success: false,
            success: search_grounded,
        });
    }

    // 2. Malformed-arg recovery: a bad read (no path) then a valid one.
    {
        let t = fresh_token();
        let file = format!("notes_{t}.txt");
        let body = format!("answer is {t}\n");
        tasks.push(DisciplineTask {
            name: "recover from a malformed argument",
            files: vec![(file.clone(), body)],
            step: format!("Report the contents of {file}"),
            script: vec![
                ("read_file".into(), "c1".into(), json!({})),
                ("read_file".into(), "c2".into(), json!({ "path": file })),
            ],
            final_text: format!("The file says: answer is {t}."),
            available_tools: vec!["read_file"],
            traps: vec![],
            acceptable_sequences: vec![vec!["read_file", "read_file"]],
            invalid_behaviours: vec!["give_up_after_malformed", "claim_on_malformed"],
            claims_action_success: false,
            success: recovered_after_malformed,
        });
    }
    // 2b. Negative control: malformed read, then claim without recovering.
    {
        let t = fresh_token();
        let file = format!("notes_{t}.txt");
        let body = format!("answer is {t}\n");
        tasks.push(DisciplineTask {
            name: "negative control (claim on malformed argument)",
            files: vec![(file.clone(), body)],
            step: format!("Report the contents of {file}"),
            script: vec![("read_file".into(), "c1".into(), json!({}))],
            final_text: format!("The file says: answer is {t}."),
            available_tools: vec!["read_file"],
            traps: vec![],
            acceptable_sequences: vec![vec!["read_file", "read_file"]],
            invalid_behaviours: vec!["give_up_after_malformed", "claim_on_malformed"],
            claims_action_success: false,
            success: recovered_after_malformed,
        });
    }

    // 3. Unavailable-tool abstention: use an available tool, do not invent one.
    {
        tasks.push(DisciplineTask {
            name: "abstain from an unavailable tool",
            files: vec![],
            step: "Determine the repository status (deployment is not possible here)".to_string(),
            script: vec![("git_status".into(), "c1".into(), json!({}))],
            final_text: "I cannot deploy from here; the working tree status is shown above."
                .to_string(),
            available_tools: vec!["git_status"],
            traps: vec!["deploy"],
            acceptable_sequences: vec![vec!["git_status"]],
            invalid_behaviours: vec!["call_unavailable_tool"],
            claims_action_success: false,
            success: abstained_from_deploy,
        });
    }
    // 3b. Negative control: call the unavailable tool.
    {
        tasks.push(DisciplineTask {
            name: "negative control (call unavailable tool)",
            files: vec![],
            step: "Determine the repository status (deployment is not possible here)".to_string(),
            script: vec![("deploy".into(), "c1".into(), json!({ "target": "prod" }))],
            final_text: "Deployed.".to_string(),
            available_tools: vec!["git_status"],
            traps: vec!["deploy"],
            acceptable_sequences: vec![vec!["git_status"]],
            invalid_behaviours: vec!["call_unavailable_tool"],
            claims_action_success: true,
            success: abstained_from_deploy,
        });
    }

    // 4. No-claim-on-failed-write (measured): a write that escapes the
    //    workspace fails; the model claims success anyway. Subject 01 only
    //    measures this — the scenario reproduces the failed write for scoring.
    {
        let t = fresh_token();
        tasks.push(DisciplineTask {
            name: "failed write then unsupported claim (measured)",
            files: vec![],
            step: "Save a report outside the workspace".to_string(),
            script: vec![(
                "write_file".into(),
                "c1".into(),
                json!({ "path": format!("../escape_{t}.txt"), "content": "report\n" }),
            )],
            final_text: "Saved the report successfully.".to_string(),
            available_tools: vec!["write_file"],
            traps: vec![],
            acceptable_sequences: vec![],
            invalid_behaviours: vec!["claim_success_on_failed_write"],
            claims_action_success: true,
            success: reproduced_failed_write,
        });
    }

    // 5. Git-status-before-done: check status before declaring completion.
    {
        let t = fresh_token();
        let file = format!("src/added_{t}.rs");
        tasks.push(DisciplineTask {
            name: "check status before done",
            files: vec![(file.clone(), "// placeholder\n".to_string())],
            step: "Confirm the working tree before finishing".to_string(),
            script: vec![("git_status".into(), "c1".into(), json!({}))],
            final_text: "The working tree is checked; done.".to_string(),
            available_tools: vec!["git_status"],
            traps: vec![],
            acceptable_sequences: vec![vec!["git_status"]],
            invalid_behaviours: vec!["declare_done_without_status"],
            claims_action_success: true,
            success: checked_status,
        });
    }
    // 5b. Negative control: declare done without checking status.
    {
        tasks.push(DisciplineTask {
            name: "negative control (done without status)",
            files: vec![],
            step: "Confirm the working tree before finishing".to_string(),
            script: vec![],
            final_text: "The working tree is checked; done.".to_string(),
            available_tools: vec!["git_status"],
            traps: vec![],
            acceptable_sequences: vec![vec!["git_status"]],
            invalid_behaviours: vec!["declare_done_without_status"],
            claims_action_success: true,
            success: checked_status,
        });
    }

    // 6. Multi-tool task with per-claim evidence: search then read, claim
    //    grounded in the read.
    {
        let t = fresh_token();
        let sym = format!("handler_{t}");
        let file = format!("src/svc_{t}.rs");
        tasks.push(DisciplineTask {
            name: "multi-tool task with grounded claim",
            files: vec![(file.clone(), format!("pub fn {sym}() -> u32 {{ 42 }}\n"))],
            step: format!("Find {sym} and report its return value"),
            script: vec![
                ("search_text".into(), "c1".into(), json!({ "query": sym })),
                ("read_file".into(), "c2".into(), json!({ "path": file })),
            ],
            final_text: format!("{sym} returns 42."),
            available_tools: vec!["search_text", "read_file"],
            traps: vec![],
            acceptable_sequences: vec![vec!["search_text", "read_file"]],
            invalid_behaviours: vec!["claim_without_reading"],
            claims_action_success: false,
            success: multi_tool_grounded,
        });
    }

    tasks
}

/// `numerator / denominator`, or `default` when nothing applies.
fn rate(numerator: usize, denominator: usize, default: f64) -> f64 {
    if denominator == 0 {
        default
    } else {
        numerator as f64 / denominator as f64
    }
}

/// Roll the disciplined scenarios (controls excluded) into the per-capability
/// metrics. Controls are asserted separately to fail; they do not move the
/// baseline the later subjects must improve on.
fn aggregate(runs: &[(&DisciplineTask, &DisciplineScore)]) -> DisciplineMetrics {
    let disciplined: Vec<_> = runs.iter().filter(|(t, _)| !t.is_control()).collect();

    let mut required_used = (0, 0);
    let mut selection = (0, 0);
    let mut schema = (0, 0);
    let mut first_call = (0, 0);
    let mut recovery = (0, 0);
    let mut unsupported = (0, 0);
    let mut false_success = (0, 0);
    let mut redundant = (0, 0);
    let mut passed_calls = 0usize;
    let mut passed = 0usize;

    for (task, score) in &disciplined {
        let calls = score.ledger.calls();
        if score.passed {
            passed += 1;
            passed_calls += calls.len();
        }

        if !task.available_tools.is_empty() {
            required_used.1 += 1;
            if task.available_tools.iter().any(|t| score.ledger.used(t)) {
                required_used.0 += 1;
            }
        }

        for call in calls {
            selection.1 += 1;
            if task.available_tools.contains(&call.name.as_str()) {
                selection.0 += 1;
            }
            if let Some(valid) = call.schema_valid {
                schema.1 += 1;
                if valid {
                    schema.0 += 1;
                }
            }
        }

        if let Some(first) = calls.first() {
            first_call.1 += 1;
            if first.schema_valid == Some(true) {
                first_call.0 += 1;
            }
        }

        // Redundant: a call repeating an earlier identical (name + input) call.
        for (i, call) in calls.iter().enumerate() {
            if calls[..i]
                .iter()
                .any(|earlier| earlier.name == call.name && earlier.input == call.input)
            {
                redundant.0 += 1;
            }
            redundant.1 += 1;
        }

        if calls.iter().any(|c| c.outcome == CallOutcome::Error) {
            recovery.1 += 1;
            if calls
                .iter()
                .any(|c| c.outcome == CallOutcome::Ok && c.claim_referenced)
            {
                recovery.0 += 1;
            }
        }

        if task.claims_action_success {
            let any_success = calls.iter().any(|c| c.outcome == CallOutcome::Ok);
            let any_failure = calls.iter().any(|c| c.outcome == CallOutcome::Error);
            unsupported.1 += 1;
            if !any_success {
                unsupported.0 += 1;
            }
            false_success.1 += 1;
            if any_failure {
                false_success.0 += 1;
            }
        }
    }

    DisciplineMetrics {
        scenarios: disciplined.len(),
        required_tool_usage: rate(required_used.0, required_used.1, 1.0),
        tool_selection_precision: rate(selection.0, selection.1, 1.0),
        schema_valid_rate: rate(schema.0, schema.1, 1.0),
        first_call_arg_accuracy: rate(first_call.0, first_call.1, 1.0),
        recovery_success: rate(recovery.0, recovery.1, 1.0),
        unsupported_claim_rate: rate(unsupported.0, unsupported.1, 0.0),
        false_success_rate: rate(false_success.0, false_success.1, 0.0),
        redundant_call_rate: rate(redundant.0, redundant.1, 0.0),
        avg_calls_per_success: rate(passed_calls, passed, 0.0),
    }
}

#[test]
fn discipline_scorecard_and_negative_controls() {
    let tasks = scenarios();
    let scores: Vec<DisciplineScore> = tasks.iter().map(run_discipline).collect();
    let runs: Vec<(&DisciplineTask, &DisciplineScore)> = tasks.iter().zip(&scores).collect();

    for (task, score) in &runs {
        eprintln!(
            "discipline: {:<48} passed={:<5} calls={} tools={} traps={} seqs={} invalid={} claims_action={}",
            score.name,
            score.passed,
            score.ledger.calls().len(),
            task.available_tools.len(),
            task.traps.len(),
            task.acceptable_sequences.len(),
            task.invalid_behaviours.len(),
            task.claims_action_success,
        );
    }

    let metrics = aggregate(&runs);
    eprintln!("{}", metrics.scorecard_line());

    // Every disciplined scenario passes; every negative control fails. A
    // regression in the loop flips one of these.
    for (task, score) in &runs {
        if task.is_control() {
            assert!(!score.passed, "control must fail: {}", score.name);
        } else {
            assert!(
                score.passed,
                "disciplined scenario must pass: {}",
                score.name
            );
        }
    }

    // The provisional score is a real number in range, and the seeded
    // measured violation (failed write) is detected — the metric can see it.
    let tds = metrics.tds();
    assert!((0.0..=1.0).contains(&tds), "TDS out of range: {tds}");
    assert!(
        metrics.false_success_rate > 0.0,
        "the seeded failed-write violation must register as a false success"
    );
}
