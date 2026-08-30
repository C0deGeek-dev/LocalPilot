//! First-party capability corpus: original tasks authored for this repository
//! (never copied from an external benchmark), each a small buggy unit with its
//! own failing→passing test. The runner materializes a task's base workspace,
//! drives the harness loop headless to produce a fix, captures the diff + emits
//! the capability scorecard, and grades by compiling and running the task's own
//! test in isolation.
//!
//! Offline (default) the loop is driven by the scripted [`FakeProvider`] applying
//! the gold solution, which proves the runner mechanics deterministically for CI.
//! A live model path is gated behind `LOCALPILOT_LIVE_TESTS`.
//!
//! A second piece — [`mine_fix_commit`] — is the corpus-extraction helper: it
//! scans a repository's history for the commit that flips a grader red→green and
//! emits a reviewable fixture stub a human curates into a task.
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use localpilot_config::{load, CliOverrides, ConfigPaths};
use localpilot_harness::{
    complexity_delta_in_diff, extract_process, judge_prompt, resume_one_step, speed_from_events,
    tests_added_in_diff, DiffStat, EvidenceLedger, Judge, JudgeCache, JudgeInput, QualityBlock,
    ResultsBlock, RuleEngine, Scorecard, SessionConfig, SessionRuntime, SCORECARD_SCHEMA,
};
use localpilot_llm::{FakeProvider, ModelProvider, ProviderRegistry};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use serde_json::Value;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

// --- fixtures ----------------------------------------------------------------

/// One first-party task loaded from `tests/corpus/<id>/`.
struct FirstPartyTask {
    id: String,
    problem: String,
    entry: String,
    base: String,
    gold: String,
}

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

/// Load every task directory under `tests/corpus/`, sorted by id for a stable
/// run order.
fn load_corpus() -> Vec<FirstPartyTask> {
    let mut tasks: Vec<FirstPartyTask> = std::fs::read_dir(corpus_dir())
        .unwrap()
        .filter_map(|entry| {
            let dir = entry.unwrap().path();
            if !dir.is_dir() {
                return None;
            }
            let manifest: Value =
                serde_json::from_str(&std::fs::read_to_string(dir.join("task.json")).unwrap())
                    .unwrap();
            let entry_name = manifest["entry"].as_str().unwrap().to_string();
            Some(FirstPartyTask {
                id: manifest["id"].as_str().unwrap().to_string(),
                problem: manifest["problem"].as_str().unwrap().to_string(),
                base: std::fs::read_to_string(dir.join("base").join(&entry_name)).unwrap(),
                gold: std::fs::read_to_string(dir.join("gold").join(&entry_name)).unwrap(),
                entry: entry_name,
            })
        })
        .collect();
    tasks.sort_by(|a, b| a.id.cmp(&b.id));
    tasks
}

// --- the isolated grader -----------------------------------------------------

/// The result of compiling and running a task's bundled test.
struct GradeResult {
    passed: bool,
    total: u32,
    passed_count: u32,
}

/// Build `content` as a tiny self-contained crate and run its bundled tests with
/// `cargo test` in an isolated temp dir, so grading never touches the loop's git
/// workspace. `cargo` is used rather than a bare `rustc` invocation because it
/// sets up the platform linker correctly on every tier-1 toolchain (a raw
/// `rustc` link from a grandchild process is environment-fragile on
/// `windows-gnu`). Red (a failing or non-compiling unit) yields `passed = false`;
/// green yields `passed = true`.
fn grade(entry: &str, content: &str) -> GradeResult {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(entry), content).unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        format!(
            "[package]\nname = \"corpus_task\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n\
             [lib]\npath = \"{entry}\"\n"
        ),
    )
    .unwrap();

    let run = Command::new("cargo")
        .args(["test", "--offline", "--quiet"])
        .current_dir(dir.path())
        .env("CARGO_TARGET_DIR", dir.path().join("target"))
        .env_remove("RUSTFLAGS")
        .output()
        .unwrap();
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    let (total, passed_count) = parse_test_counts(&combined);
    GradeResult {
        passed: run.status.success(),
        total,
        passed_count,
    }
}

/// Pull `N passed; M failed` out of a libtest summary line (e.g.
/// `test result: ok. 3 passed; 0 failed; …`).
fn parse_test_counts(output: &str) -> (u32, u32) {
    for line in output.lines() {
        if let Some(rest) = line.trim().strip_prefix("test result:") {
            let passed = scan_count(rest, "passed");
            let failed = scan_count(rest, "failed");
            return (passed + failed, passed);
        }
    }
    (0, 0)
}

/// The integer token immediately preceding `label` in a libtest summary, e.g.
/// `scan_count("ok. 3 passed; 0 failed", "passed") == 3`.
fn scan_count(summary: &str, label: &str) -> u32 {
    let tokens: Vec<&str> = summary
        .split(|c: char| c.is_whitespace() || c == ';' || c == '.')
        .filter(|t| !t.is_empty())
        .collect();
    tokens
        .iter()
        .position(|t| *t == label)
        .filter(|&i| i > 0)
        .and_then(|i| tokens[i - 1].parse::<u32>().ok())
        .unwrap_or(0)
}

// --- the in-repo capability runner -------------------------------------------

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

/// The gold diff (base → gold) as a [`DiffStat`], computed with `git diff
/// --no-index` so the vs-gold ratio has a real reference.
fn gold_diff_stat(task: &FirstPartyTask) -> DiffStat {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base.rs");
    let gold = dir.path().join("gold.rs");
    std::fs::write(&base, &task.base).unwrap();
    std::fs::write(&gold, &task.gold).unwrap();
    // `git diff --no-index` exits non-zero when the files differ; we read stdout.
    let out = Command::new("git")
        .args(["diff", "--no-color", "--no-index"])
        .arg(&base)
        .arg(&gold)
        .output()
        .unwrap();
    DiffStat::from_unified(&String::from_utf8_lossy(&out.stdout))
}

/// Attach an offline judge block from a seeded cache, proving the LLM-as-judge
/// integrates with the scorecard deterministically. The cached response stands in
/// for a real judgment (which comes from the live judge model), so CI needs no
/// model. The scores are blind by construction — the prompt carries no arm id.
fn offline_judge(diff_text: &str) -> Option<localpilot_harness::JudgeBlock> {
    let input = JudgeInput {
        diff: diff_text,
        trajectory: None,
    };
    let mut cache = JudgeCache::default();
    // Seed the ranking fixtures so the judge passes its self-test (every `better`
    // outscores its `worse`), then the task diff. The corpus run scores through the
    // gate (`score_offline_gated`), exercising the "prove the judge ranks before
    // trusting it" path offline — an inverted judge would refuse here.
    for fx in localpilot_harness::RANKING_FIXTURES {
        cache.insert(
            &judge_prompt(&JudgeInput {
                diff: fx.better,
                trajectory: None,
            }),
            "{\"readability\":5,\"idiomaticity\":5,\"abstraction_fit\":5,\"bug_resistance\":5}",
        );
        cache.insert(
            &judge_prompt(&JudgeInput {
                diff: fx.worse,
                trajectory: None,
            }),
            "{\"readability\":2,\"idiomaticity\":2,\"abstraction_fit\":2,\"bug_resistance\":2}",
        );
    }
    cache.insert(
        &judge_prompt(&input),
        "{\"readability\":4,\"idiomaticity\":4,\"abstraction_fit\":4,\"bug_resistance\":4}",
    );
    // The seeded judge passes its ranking self-test, so the gate returns a score;
    // an inverted judge would return `Err(Untrustworthy)` here instead.
    Judge::new("offline-judge-fixture", cache)
        .score_offline_gated(&input)
        .unwrap()
}

/// Drive one task offline: materialize the base workspace, confirm it is red,
/// run the loop (the fake provider applies the gold fix), confirm it is green,
/// and emit the capability scorecard.
fn run_offline(task: &FirstPartyTask) -> Scorecard {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join(&task.entry), &task.base).unwrap();
    std::fs::write(
        root.join("PROGRESS.md"),
        format!(
            "# Progress: corpus\nBranch: feature/corpus\n\n## Steps\n\n- [ ] 1. {}\n",
            task.problem
        ),
    )
    .unwrap();

    git(root, &["init"]);
    git(root, &["config", "user.email", "corpus@example.com"]);
    git(root, &["config", "user.name", "Corpus"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "initial"]);
    let base_ref = git_out(root, &["rev-parse", "HEAD"]).trim().to_string();

    // The base workspace must fail its own test — that is the bug we are fixing.
    let base_grade = grade(&task.entry, &task.base);
    assert!(
        !base_grade.passed,
        "base for `{}` must be red before the fix",
        task.id
    );

    // The fake provider applies the gold solution; the harness loop runs it
    // through the real session/commit machinery.
    let provider = FakeProvider::new()
        .tool_call(
            "fix",
            "write_file",
            serde_json::json!({ "path": task.entry, "content": task.gold }),
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
        .block_on(resume_one_step(&mut runtime, root, &rules, None, &[], 3))
        .unwrap();
    let wall_ms = started.elapsed().as_millis() as u64;
    assert!(outcome.committed, "the corpus step should commit the fix");

    // Grade the produced workspace in isolation.
    let produced = std::fs::read_to_string(root.join(&task.entry)).unwrap();
    let result = grade(&task.entry, &produced);

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
            ":!DECISIONS.md",
        ],
    );
    let diff = DiffStat::from_unified(&diff_text);
    let gold = gold_diff_stat(task);
    let events = Store::open(root).read_events(session).unwrap();
    let ledger = EvidenceLedger::project(&events);

    Scorecard {
        schema: SCORECARD_SCHEMA,
        task: task.id.clone(),
        arm: "offline".to_string(),
        model: "fake".to_string(),
        results: ResultsBlock {
            passed: result.passed,
            regression_safe: result.passed,
            partial_credit: if result.passed { 1.0 } else { 0.0 },
            tests_total: result.total,
            tests_passed: result.passed_count,
        },
        quality: QualityBlock::from_signals(
            &diff,
            Some(&gold),
            &outcome.gate,
            Some(complexity_delta_in_diff(&diff_text)),
            tests_added_in_diff(&diff_text),
        ),
        process: extract_process(&events, &ledger),
        speed: speed_from_events(&events, wall_ms),
        judge: offline_judge(&diff_text),
    }
}

#[test]
fn first_party_corpus_offline_scorecards() {
    let corpus = load_corpus();
    assert!(
        corpus.len() >= 5,
        "the seeded corpus should carry at least five tasks (found {})",
        corpus.len()
    );

    let mut solved = 0usize;
    for task in &corpus {
        let card = run_offline(task);
        eprintln!("first-party: {:<22} {}", task.id, card.to_json().unwrap());

        // The scorecard is well-formed and the offline run solved the task.
        assert_eq!(card.schema, SCORECARD_SCHEMA);
        let round: Scorecard = serde_json::from_str(&card.to_json().unwrap()).unwrap();
        assert_eq!(card, round);
        assert!(card.results.passed, "offline gold fix solves `{}`", task.id);
        assert!(card.results.tests_total > 0, "graded by real tests");
        assert!(card.quality.diff_added > 0, "the fix changed code");
        let judge = card
            .judge
            .as_ref()
            .expect("an offline judge block is attached");
        assert!(
            (1..=5).contains(&judge.readability),
            "judge scores are in range"
        );
        assert!(
            judge.blinded,
            "single-solution judging is blind by construction"
        );
        // The offline run applies the gold patch verbatim, so churn matches gold.
        assert_eq!(
            card.quality.vs_gold_ratio,
            Some(1.0),
            "offline candidate churn equals gold for `{}`",
            task.id
        );
        assert_eq!(card.process.exit_reason, "Done");
        if card.results.passed {
            solved += 1;
        }
    }
    assert_eq!(solved, corpus.len(), "every offline task is solved");
}

// --- corpus-extraction helper ------------------------------------------------

/// A reviewable candidate task mined from history: the base/fix commits and the
/// files the fix touched. A human curates a stub into a `tests/corpus/` task.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FixtureStub {
    base_ref: String,
    fix_ref: String,
    files_changed: Vec<String>,
}

/// Scan a repository's (linear) history for the first commit that flips `grader`
/// from red at its parent to green at the commit — the shape of a bug-fix commit
/// with a test. `grader` is handed a read-only checkout of each commit and
/// returns whether it passes. Returns `None` when no such transition exists.
fn mine_fix_commit(repo: &Path, grader: impl Fn(&Path) -> bool) -> Option<FixtureStub> {
    let log = git_out(repo, &["log", "--format=%H", "--reverse"]);
    let commits: Vec<String> = log.lines().map(str::to_string).collect();
    for pair in commits.windows(2) {
        let (parent, child) = (&pair[0], &pair[1]);
        if !grade_commit(repo, parent, &grader) && grade_commit(repo, child, &grader) {
            let names = git_out(repo, &["diff", "--name-only", parent, child]);
            return Some(FixtureStub {
                base_ref: parent.clone(),
                fix_ref: child.clone(),
                files_changed: names.lines().map(str::to_string).collect(),
            });
        }
    }
    None
}

/// Materialize `commit` into a throwaway worktree and grade it.
fn grade_commit(repo: &Path, commit: &str, grader: &impl Fn(&Path) -> bool) -> bool {
    let work = tempfile::tempdir().unwrap();
    let work_path = work.path().to_string_lossy().into_owned();
    // A detached worktree gives a clean read-only checkout of the commit.
    git(repo, &["worktree", "add", "--detach", &work_path, commit]);
    let verdict = grader(work.path());
    git(repo, &["worktree", "remove", "--force", &work_path]);
    verdict
}

#[test]
fn mining_helper_finds_the_fix_commit() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init"]);
    git(repo, &["config", "user.email", "mine@example.com"]);
    git(repo, &["config", "user.name", "Mine"]);

    // Commit 1: an unrelated file. Commit 2: introduces the bug. Commit 3: fixes
    // it. The helper must pinpoint the 2→3 transition.
    std::fs::write(repo.join("readme.txt"), "hello\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-m", "seed"]);

    std::fs::write(repo.join("value.txt"), "broken\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-m", "add value (broken)"]);

    std::fs::write(repo.join("value.txt"), "fixed\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-m", "fix value"]);

    let grader = |work: &Path| {
        std::fs::read_to_string(work.join("value.txt"))
            .map(|s| s.trim() == "fixed")
            .unwrap_or(false)
    };
    let stub = mine_fix_commit(repo, grader).expect("a fix transition is found");
    assert_eq!(stub.files_changed, vec!["value.txt".to_string()]);
    // The fix commit is HEAD; its parent is the broken commit.
    let head = git_out(repo, &["rev-parse", "HEAD"]).trim().to_string();
    let parent = git_out(repo, &["rev-parse", "HEAD~1"]).trim().to_string();
    assert_eq!(stub.fix_ref, head);
    assert_eq!(stub.base_ref, parent);
}

// --- gated live path ---------------------------------------------------------

/// Drive one task against a real model: the model is given the problem statement
/// and must produce the fix using the workspace tools; the runner grades the
/// result exactly like the offline path.
async fn run_live(
    task: &FirstPartyTask,
    provider: Arc<dyn ModelProvider>,
    model: String,
) -> Scorecard {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join(&task.entry), &task.base).unwrap();
    git(root, &["init"]);
    git(root, &["config", "user.email", "corpus@example.com"]);
    git(root, &["config", "user.name", "Corpus"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-m", "initial"]);
    let base_ref = git_out(root, &["rev-parse", "HEAD"]).trim().to_string();

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
    let session = runtime.session_id();

    let (events_tx, _rx) = broadcast::channel(256);
    let cancel = CancellationToken::new();
    let prompt = format!(
        "The file `{}` in this workspace has a bug. {} Edit the file with the available \
         tools so its bundled tests pass.",
        task.entry, task.problem
    );
    let started = std::time::Instant::now();
    let _ = runtime.run_turn(&prompt, &events_tx, &cancel).await;
    let wall_ms = started.elapsed().as_millis() as u64;

    let produced = std::fs::read_to_string(root.join(&task.entry)).unwrap_or_default();
    let result = grade(&task.entry, &produced);

    git(root, &["add", "-A"]);
    let diff_text = git_out(
        root,
        &[
            "diff",
            "--no-color",
            "--cached",
            &base_ref,
            "--",
            ".",
            ":!PROGRESS.md",
            ":!DECISIONS.md",
        ],
    );
    let diff = DiffStat::from_unified(&diff_text);
    let gold = gold_diff_stat(task);
    let events = Store::open(root).read_events(session).unwrap();
    let ledger = EvidenceLedger::project(&events);

    Scorecard {
        schema: SCORECARD_SCHEMA,
        task: task.id.clone(),
        arm: "live".to_string(),
        model: runtime_model(&events),
        results: ResultsBlock {
            passed: result.passed,
            regression_safe: result.passed,
            partial_credit: if result.passed { 1.0 } else { 0.0 },
            tests_total: result.total,
            tests_passed: result.passed_count,
        },
        quality: QualityBlock::from_signals(
            &diff,
            Some(&gold),
            &[],
            Some(complexity_delta_in_diff(&diff_text)),
            tests_added_in_diff(&diff_text),
        ),
        process: extract_process(&events, &ledger),
        speed: speed_from_events(&events, wall_ms),
        judge: None,
    }
}

/// The model recorded on the first turn, for the scorecard's `model` field.
fn runtime_model(events: &[localpilot_store::SessionEvent]) -> String {
    events
        .iter()
        .find_map(|e| match &e.kind {
            localpilot_store::SessionEventKind::TurnStarted { model } => Some(model.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "live".to_string())
}

/// Live capability run across the whole corpus: a real model fixes each task and
/// the runner grades it. Off by default; skips cleanly when no provider/model is
/// configured, so offline CI stays deterministic (validation-evidence policy).
///   `LOCALPILOT_LIVE_TESTS=1 [LOCALPILOT_LIVE_MODEL=<model>] cargo test -p localpilot-harness --test first_party -- --nocapture`
#[test]
fn first_party_live_is_gated() {
    if std::env::var("LOCALPILOT_LIVE_TESTS").is_err() {
        eprintln!("skipping live first-party corpus: set LOCALPILOT_LIVE_TESTS to enable");
        return;
    }
    let cwd = std::env::current_dir().unwrap();
    let Ok(config) = load(&ConfigPaths::standard(&cwd), &CliOverrides::default()) else {
        eprintln!("skipping live first-party corpus: config load failed");
        return;
    };
    let Ok(registry) = ProviderRegistry::from_config(&config) else {
        eprintln!("skipping live first-party corpus: provider configuration is incomplete");
        return;
    };
    let Some(provider) = registry.default_provider().cloned() else {
        eprintln!("skipping live first-party corpus: no default provider is configured");
        return;
    };
    let Some(model) = std::env::var("LOCALPILOT_LIVE_MODEL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| config.resolve_model(None))
    else {
        eprintln!("skipping live first-party corpus: set provider.model or LOCALPILOT_LIVE_MODEL");
        return;
    };

    let rt = tokio::runtime::Runtime::new().unwrap();
    let corpus = load_corpus();
    let mut solved = 0usize;
    for task in &corpus {
        let card = rt.block_on(run_live(task, provider.clone(), model.clone()));
        eprintln!(
            "live first-party: {:<22} {}",
            task.id,
            card.to_json().unwrap()
        );
        if card.results.passed {
            solved += 1;
        }
    }
    eprintln!(
        "live first-party corpus: {solved}/{} solved (model={model})",
        corpus.len()
    );
}
