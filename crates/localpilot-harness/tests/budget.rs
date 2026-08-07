//! The tool-call budget stops a runaway tool loop cleanly with a recorded
//! reason, while a normal task stays well under the ceiling.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use localpilot_harness::{RuntimeEvent, SessionConfig, SessionRuntime, StopReason};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use serde_json::json;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

fn runtime_budgets(
    root: &std::path::Path,
    provider: FakeProvider,
    soft_start: usize,
    hard_max: usize,
) -> SessionRuntime {
    SessionRuntime::new(
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
            tool_call_budget: Some(soft_start),
            tool_call_budget_max: Some(hard_max),
            // An operator-set budget: the cost controller owns the no-progress
            // stop, so the always-on guard defers to it.
            tool_budget_explicit: true,
            ..SessionConfig::default()
        },
        Vec::new(),
    )
}

/// A runtime mirroring the built-in default rail (ADR-0055): the resolver fills
/// `tool_call_budget_max` with no soft start and marks the budget *not*
/// operator-explicit. The `soft == hard` collapse disables the controller's
/// no-progress branch, so the always-on degenerate-loop guard must stay active.
fn runtime_builtin_default(
    root: &std::path::Path,
    provider: FakeProvider,
    hard_max: usize,
) -> SessionRuntime {
    SessionRuntime::new(
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
            tool_call_budget: None,
            tool_call_budget_max: Some(hard_max),
            tool_budget_explicit: false,
            ..SessionConfig::default()
        },
        Vec::new(),
    )
}

/// A flat fixed ceiling (`max == soft start`): the pre-adaptive behaviour, used
/// by the cost-ceiling tests.
fn runtime(root: &std::path::Path, provider: FakeProvider, budget: usize) -> SessionRuntime {
    runtime_budgets(root, provider, budget, budget)
}

/// A runtime with the tool-call budget left at its default (off), to exercise the
/// always-on degenerate-loop guard rather than the opt-in cost budget.
fn runtime_no_budget(root: &std::path::Path, provider: FakeProvider) -> SessionRuntime {
    SessionRuntime::new(
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
            // tool_call_budget / tool_call_budget_max left at their default None.
            ..SessionConfig::default()
        },
        Vec::new(),
    )
}

#[test]
fn a_runaway_tool_loop_hits_the_budget_and_stops() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("f.txt"), "x\n").unwrap();

    // Five scripted read calls against a budget of three: the loop runs three,
    // then stops at the fourth before it executes.
    let mut provider = FakeProvider::new();
    for _ in 0..5 {
        provider = provider.tool_call("c", "read_file", json!({ "path": "f.txt" }));
    }
    provider = provider.text("done");

    let mut runtime = runtime(root, provider, 3);
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn("read it repeatedly", &events, &cancel));

    assert_eq!(
        reason,
        StopReason::BudgetExceeded,
        "the runaway loop must stop on the budget, not run unbounded"
    );
}

#[test]
fn a_normal_task_stays_under_a_configured_budget() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("f.txt"), "x\n").unwrap();

    // One tool call then a final answer — far under a configured ceiling of 50.
    let provider = FakeProvider::new()
        .tool_call("c1", "read_file", json!({ "path": "f.txt" }))
        .text("the file says x");

    let mut runtime = runtime(root, provider, 50);
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn("read the file", &events, &cancel));

    assert_eq!(
        reason,
        StopReason::Done,
        "a normal task finishes normally, not on the budget"
    );
}

#[test]
fn an_unset_budget_still_halts_a_spinning_loop() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("f.txt"), "x\n").unwrap();

    // The same read repeats many times. The opt-in cost budget is off, but the
    // always-on degenerate-loop guard still stops a turn the no-progress detector
    // flags as spinning — so a repeat-call loop can no longer run unbounded the
    // way it did before the guard. See ADR-0052.
    let mut provider = FakeProvider::new();
    for _ in 0..40 {
        provider = provider.tool_call("c", "read_file", json!({ "path": "f.txt" }));
    }
    provider = provider.text("done");

    let mut runtime = runtime_no_budget(root, provider);
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn("read it repeatedly", &events, &cancel));

    assert_eq!(
        reason,
        StopReason::NoProgress,
        "with the budget off, the always-on guard still halts a spinning loop"
    );
}

#[test]
fn an_unset_budget_halts_a_run_of_failing_calls() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Many calls that fail the same way (reading a file that does not exist). The
    // no-progress detector is fed only by successful calls, so it never sees this
    // denied/failing spin — the always-on consecutive-failure guard catches it
    // even with the budget off, instead of running every scripted call.
    let mut provider = FakeProvider::new();
    for _ in 0..40 {
        provider = provider.tool_call("c", "read_file", json!({ "path": "missing.txt" }));
    }
    provider = provider.text("gave up");

    let mut runtime = runtime_no_budget(root, provider);
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason =
        rt.block_on(runtime.run_turn("read the missing file repeatedly", &events, &cancel));

    assert_eq!(
        reason,
        StopReason::NoProgress,
        "a run of consecutive failing calls halts under the always-on guard"
    );
}

#[test]
fn an_unset_budget_does_not_cut_a_productive_turn() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Thirty *distinct* reads, each making progress: the no-progress detector
    // never trips and there are no failures, so the always-on guard must not fire
    // — a long but productive turn still finishes on its own answer.
    let mut provider = FakeProvider::new();
    for i in 0..30 {
        let name = format!("f{i}.txt");
        std::fs::write(root.join(&name), format!("contents {i}\n")).unwrap();
        provider = provider.tool_call(&format!("c{i}"), "read_file", json!({ "path": name }));
    }
    provider = provider.text("read them all");

    let mut runtime = runtime_no_budget(root, provider);
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn("read each file", &events, &cancel));

    assert_eq!(
        reason,
        StopReason::Done,
        "a productive turn is never cut by the always-on guard"
    );
}

#[test]
fn a_productive_turn_runs_past_the_soft_start_when_the_max_is_higher() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // Five *distinct* reads (different files, different contents): each call
    // makes progress, so the no-progress detector never trips and the turn runs
    // past a soft start of 3 up to the higher max of 10.
    let mut provider = FakeProvider::new();
    for i in 0..5 {
        let name = format!("f{i}.txt");
        std::fs::write(root.join(&name), format!("contents {i}\n")).unwrap();
        provider = provider.tool_call(&format!("c{i}"), "read_file", json!({ "path": name }));
    }
    provider = provider.text("read them all");

    let mut runtime = runtime_budgets(root, provider, 3, 10);
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn("read each file", &events, &cancel));

    assert_eq!(
        reason,
        StopReason::Done,
        "a turn that keeps making progress runs past the old fixed ceiling"
    );
}

#[test]
fn a_spinning_turn_stops_on_no_progress_before_the_max() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("f.txt"), "x\n").unwrap();

    // The same read repeats with the same result: no forward progress. With a
    // soft start of 3 and a high max of 50, the turn stops on the no-progress
    // path at the soft start rather than spinning up to the cost ceiling.
    let mut provider = FakeProvider::new();
    for _ in 0..6 {
        provider = provider.tool_call("c", "read_file", json!({ "path": "f.txt" }));
    }
    provider = provider.text("done");

    let mut runtime = runtime_budgets(root, provider, 3, 50);
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn("read it repeatedly", &events, &cancel));

    assert_eq!(
        reason,
        StopReason::NoProgress,
        "a spinning turn stops on no progress, distinct from the cost ceiling"
    );
}

#[test]
fn the_builtin_default_rail_halts_a_spinning_loop_on_no_progress() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("f.txt"), "x\n").unwrap();

    // The built-in default fills only `..._max` (no soft start), so the budget
    // collapses to `soft == hard` — the controller's no-progress branch is dead.
    // Without the always-on guard staying active for the built-in default, these
    // 20 identical reads would run unchecked to `done` (the regression); the guard
    // must instead stop the spin early on `NoProgress`, far below the ceiling.
    let mut provider = FakeProvider::new();
    for _ in 0..20 {
        provider = provider.tool_call("c", "read_file", json!({ "path": "f.txt" }));
    }
    provider = provider.text("done");

    let mut runtime = runtime_builtin_default(root, provider, 50);
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn("read it repeatedly", &events, &cancel));

    assert_eq!(
        reason,
        StopReason::NoProgress,
        "the built-in default keeps the always-on guard; a spin stops early, not at the ceiling"
    );
}

#[test]
fn the_builtin_default_rail_halts_a_run_of_failing_calls() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Consecutive failures (reading a missing file). The no-progress detector is
    // fed only by successful calls, so the consecutive-failure guard is the one
    // that must catch this. A ceiling of 50 sits above the 12-failure limit, so a
    // `NoProgress` stop proves the guard — not the cost cap — fired.
    let mut provider = FakeProvider::new();
    for _ in 0..20 {
        provider = provider.tool_call("c", "read_file", json!({ "path": "missing.txt" }));
    }
    provider = provider.text("gave up");

    let mut runtime = runtime_builtin_default(root, provider, 50);
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason =
        rt.block_on(runtime.run_turn("read the missing file repeatedly", &events, &cancel));

    assert_eq!(
        reason,
        StopReason::NoProgress,
        "a run of consecutive failures halts on the built-in default's always-on guard"
    );
}

#[test]
fn the_builtin_default_ceiling_still_bounds_a_nondegenerate_runaway() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // Distinct, progressing reads that defeat the no-progress signal: the guard
    // never fires, so the cost ceiling is the only bound. Twelve distinct reads
    // against a ceiling of eight must stop on the cost cap — the built-in `..._max`
    // still bounds a runaway the early guard cannot catch.
    let mut provider = FakeProvider::new();
    for i in 0..12 {
        let name = format!("f{i}.txt");
        std::fs::write(root.join(&name), format!("contents {i}\n")).unwrap();
        provider = provider.tool_call(&format!("c{i}"), "read_file", json!({ "path": name }));
    }
    provider = provider.text("read them all");

    let mut runtime = runtime_builtin_default(root, provider, 8);
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn("read each file", &events, &cancel));

    assert_eq!(
        reason,
        StopReason::BudgetExceeded,
        "a non-degenerate runaway is still capped by the built-in ceiling"
    );
}

// --- A/B measurement: adaptive controller vs the flat fixed ceiling ----------

/// One scenario's outcome: the stop reason and whether the no-progress nudge
/// fired during the turn.
struct AbOutcome {
    reason: StopReason,
    nudged: bool,
}

/// Run `provider` to completion under a `(soft, max)` budget and report the stop
/// reason plus whether the no-progress strategy-change nudge was emitted.
fn run_ab(root: &std::path::Path, provider: FakeProvider, soft: usize, max: usize) -> AbOutcome {
    let mut runtime = runtime_budgets(root, provider, soft, max);
    let (events, mut rx) = broadcast::channel(4096);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn("task", &events, &cancel));
    let mut nudged = false;
    while let Ok(event) = rx.try_recv() {
        if let RuntimeEvent::Warning(text) = event {
            if text.contains("not making forward progress") {
                nudged = true;
            }
        }
    }
    AbOutcome { reason, nudged }
}

/// A provider that reads `count` *distinct* files (each written with unique
/// contents), then answers — a turn that keeps making progress.
fn distinct_reads(root: &std::path::Path, count: usize) -> FakeProvider {
    let mut provider = FakeProvider::new();
    for i in 0..count {
        let name = format!("f{i}.txt");
        std::fs::write(root.join(&name), format!("contents {i}\n")).unwrap();
        provider = provider.tool_call(&format!("c{i}"), "read_file", json!({ "path": name }));
    }
    provider.text("read them all")
}

/// A provider that reads the *same* file `count` times — a spinning turn that
/// makes no forward progress.
fn identical_reads(root: &std::path::Path, count: usize) -> FakeProvider {
    std::fs::write(root.join("f.txt"), "x\n").unwrap();
    let mut provider = FakeProvider::new();
    for _ in 0..count {
        provider = provider.tool_call("c", "read_file", json!({ "path": "f.txt" }));
    }
    provider.text("done")
}

/// Compare the adaptive controller against the flat fixed ceiling across three
/// scenarios, print the delta, and assert the wins: a productive turn the fixed
/// ceiling would cut now completes; a spinning turn stops on a distinct
/// `NoProgress` reason after a nudge; a runaway that defeats the no-progress
/// signal is still bounded by the hard cost ceiling.
#[test]
fn adaptive_vs_fixed_ceiling_ab() {
    const SOFT: usize = 8;
    const MAX: usize = 50;

    let dir = tempfile::tempdir().unwrap();

    // (a) Productive: 12 distinct reads — more than the soft start, all progress.
    let prod_root = dir.path().join("productive");
    std::fs::create_dir_all(&prod_root).unwrap();
    let fixed_prod = run_ab(&prod_root, distinct_reads(&prod_root, 12), SOFT, SOFT);
    let adaptive_prod = run_ab(&prod_root, distinct_reads(&prod_root, 12), SOFT, MAX);

    // (b) Spinning: 12 identical reads — no forward progress.
    let spin_root = dir.path().join("spinning");
    std::fs::create_dir_all(&spin_root).unwrap();
    let fixed_spin = run_ab(&spin_root, identical_reads(&spin_root, 12), SOFT, SOFT);
    let adaptive_spin = run_ab(&spin_root, identical_reads(&spin_root, 12), SOFT, MAX);

    // (c) Runaway that defeats the no-progress signal: 60 distinct reads.
    let run_root = dir.path().join("runaway");
    std::fs::create_dir_all(&run_root).unwrap();
    let adaptive_runaway = run_ab(&run_root, distinct_reads(&run_root, 60), SOFT, MAX);

    eprintln!("adaptive-budget A/B (soft={SOFT}, max={MAX}):");
    eprintln!(
        "  productive(12 distinct): fixed={:?} adaptive={:?}",
        fixed_prod.reason, adaptive_prod.reason
    );
    eprintln!(
        "  spinning(12 identical):  fixed={:?} adaptive={:?} (nudged={})",
        fixed_spin.reason, adaptive_spin.reason, adaptive_spin.nudged
    );
    eprintln!(
        "  runaway(60 distinct):    adaptive={:?}",
        adaptive_runaway.reason
    );

    // False stops of a productive turn: the fixed ceiling cuts it; the adaptive
    // controller does not.
    let fixed_false_stops = usize::from(fixed_prod.reason != StopReason::Done);
    let adaptive_false_stops = usize::from(adaptive_prod.reason != StopReason::Done);
    eprintln!(
        "  productive false-stops: fixed={fixed_false_stops} adaptive={adaptive_false_stops}"
    );

    // The win on a productive turn: fixed cuts it at the ceiling, adaptive runs.
    assert_eq!(fixed_prod.reason, StopReason::BudgetExceeded);
    assert_eq!(adaptive_prod.reason, StopReason::Done);
    assert_eq!(
        adaptive_false_stops, 0,
        "adaptive must not cut a productive turn"
    );

    // The spinning turn: fixed stops on the generic ceiling; adaptive stops on a
    // distinct, diagnosable reason and nudged a strategy change first.
    assert_eq!(fixed_spin.reason, StopReason::BudgetExceeded);
    assert_eq!(adaptive_spin.reason, StopReason::NoProgress);
    assert!(
        adaptive_spin.nudged,
        "a spinning turn gets a strategy-change nudge"
    );

    // The cost contract holds even when the no-progress signal is defeated.
    assert_eq!(
        adaptive_runaway.reason,
        StopReason::BudgetExceeded,
        "a runaway that defeats the no-progress signal is still bounded by the cost ceiling"
    );
}

// --- Subject 02: coupled default-path grace + recoverable detector signal -----

/// One turn's observable outcome. `warnings` counts the strategy-change Warning
/// *events*; `hints` counts the model-visible no-progress hint appended to a tool
/// result's output (a distinct surface — not the synthetic User message, which is
/// the later stop notice). `tool_calls` counts executed calls (one `ToolFinished`
/// each — a call the guard stops never dispatches).
struct TurnObservation {
    reason: StopReason,
    warnings: usize,
    hints: usize,
    tool_calls: usize,
}

fn observe_turn(mut runtime: SessionRuntime, prompt: &str) -> TurnObservation {
    let (events, mut rx) = broadcast::channel(4096);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn(prompt, &events, &cancel));
    let mut warnings = 0;
    let mut hints = 0;
    let mut tool_calls = 0;
    while let Ok(event) = rx.try_recv() {
        match event {
            // The strategy-change Warning; the stop notice says "no forward
            // progress" (not "not making forward progress"), so it is excluded.
            RuntimeEvent::Warning(text) if text.contains("not making forward progress") => {
                warnings += 1;
            }
            RuntimeEvent::ToolFinished { output, .. } => {
                tool_calls += 1;
                // The model-visible hint appended to the tool result (distinct
                // marker from the Warning event above).
                if output.contains("These tool calls are not making forward progress") {
                    hints += 1;
                }
            }
            _ => {}
        }
    }
    TurnObservation {
        reason,
        warnings,
        hints,
        tool_calls,
    }
}

#[test]
fn default_rail_trips_then_a_novel_call_recovers_and_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("f.txt"), "x\n").unwrap();
    std::fs::write(root.join("g.txt"), "y\n").unwrap();

    // Three identical reads trip the detector (one Warning + one model-visible
    // hint + one grace dispatch); the grace call is a genuinely NOVEL read that
    // recomputes the dynamic signal to clear, so the turn recovers and finishes
    // on its own answer — trip → nudge → novel → Done.
    let provider = FakeProvider::new()
        .tool_call("c1", "read_file", json!({ "path": "f.txt" }))
        .tool_call("c2", "read_file", json!({ "path": "f.txt" }))
        .tool_call("c3", "read_file", json!({ "path": "f.txt" }))
        .tool_call("c4", "read_file", json!({ "path": "g.txt" }))
        .text("done");

    let obs = observe_turn(runtime_no_budget(root, provider), "read then recover");
    assert_eq!(obs.reason, StopReason::Done, "a recovered turn finishes");
    assert_eq!(obs.warnings, 1, "exactly one strategy-change Warning");
    assert_eq!(obs.hints, 1, "exactly one model-visible hint");
    assert_eq!(
        obs.tool_calls, 4,
        "three identical reads + the one novel grace dispatch execute; then Done"
    );
}

#[test]
fn default_rail_trips_then_stays_stuck_stops_after_exactly_one_grace() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("f.txt"), "x\n").unwrap();

    // Ten identical reads: the detector trips at the third (Warning + hint +
    // grace), the fourth is the single grace dispatch, and the fifth call is
    // stopped before it executes — trip → nudge → still-stuck → NoProgress.
    let mut provider = FakeProvider::new();
    for _ in 0..10 {
        provider = provider.tool_call("c", "read_file", json!({ "path": "f.txt" }));
    }
    provider = provider.text("done");

    let obs = observe_turn(runtime_no_budget(root, provider), "spin forever");
    assert_eq!(
        obs.reason,
        StopReason::NoProgress,
        "a still-stuck turn stops"
    );
    assert_eq!(obs.warnings, 1, "exactly one Warning, never a second");
    assert_eq!(obs.hints, 1, "exactly one model-visible hint");
    assert_eq!(
        obs.tool_calls, 4,
        "3 to trip + exactly 1 grace dispatch; the 5th is stopped before executing"
    );
}

#[test]
fn default_rail_failing_grace_call_stops_before_another_call() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("f.txt"), "x\n").unwrap();

    // Three identical successful reads trip/nudge (Warning + hint + grace); the
    // fourth is the grace dispatch but it FAILS (missing file), so it never
    // observes and cannot clear the signal — the next guard stops before a fifth
    // call. NoProgress, exactly four executed calls, exactly one Warning + hint.
    let provider = FakeProvider::new()
        .tool_call("c1", "read_file", json!({ "path": "f.txt" }))
        .tool_call("c2", "read_file", json!({ "path": "f.txt" }))
        .tool_call("c3", "read_file", json!({ "path": "f.txt" }))
        .tool_call("c4", "read_file", json!({ "path": "missing.txt" }))
        .tool_call("c5", "read_file", json!({ "path": "f.txt" }))
        .text("done");

    let obs = observe_turn(runtime_no_budget(root, provider), "grace then fail");
    assert_eq!(
        obs.reason,
        StopReason::NoProgress,
        "the failing grace call stops the turn"
    );
    assert_eq!(obs.warnings, 1, "exactly one Warning");
    assert_eq!(obs.hints, 1, "exactly one model-visible hint");
    assert_eq!(
        obs.tool_calls, 4,
        "3 to trip + the 1 failing grace dispatch; the 5th is stopped before executing"
    );
}

#[test]
fn default_rail_recovered_then_retripped_stops_normally() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("f.txt"), "x\n").unwrap();
    std::fs::write(root.join("g.txt"), "y\n").unwrap();

    // Trip on three identical reads (Warning + hint + grace), recover on a novel
    // read (the grace dispatch clears the dynamic signal), then re-trip on the
    // same repeated read. Because the nudge and grace are already spent, the next
    // offered call is stopped normally — no second Warning/hint, no second grace.
    let provider = FakeProvider::new()
        .tool_call("c1", "read_file", json!({ "path": "f.txt" }))
        .tool_call("c2", "read_file", json!({ "path": "f.txt" }))
        .tool_call("c3", "read_file", json!({ "path": "f.txt" }))
        .tool_call("c4", "read_file", json!({ "path": "g.txt" }))
        .tool_call("c5", "read_file", json!({ "path": "f.txt" }))
        .tool_call("c6", "read_file", json!({ "path": "f.txt" }))
        .text("done");

    let obs = observe_turn(runtime_no_budget(root, provider), "recover then re-trip");
    assert_eq!(
        obs.reason,
        StopReason::NoProgress,
        "the re-tripped turn stops"
    );
    assert_eq!(obs.warnings, 1, "the spent nudge never re-fires");
    assert_eq!(obs.hints, 1, "the spent hint never re-fires");
    assert_eq!(
        obs.tool_calls, 5,
        "3 trip + 1 novel grace (recover) + 1 re-trip; the 6th is stopped before executing"
    );
}

#[test]
fn default_rail_consecutive_failures_get_no_grace() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // A run of failing calls (reading a missing file). The no-progress detector
    // is fed only by successful calls, so it never warns and never mints a grace
    // here — the consecutive-failure backstop stops the turn on its own limit,
    // with zero Warnings/hints and no extra grace dispatch.
    let mut provider = FakeProvider::new();
    for _ in 0..40 {
        provider = provider.tool_call("c", "read_file", json!({ "path": "missing.txt" }));
    }
    provider = provider.text("gave up");

    let obs = observe_turn(runtime_no_budget(root, provider), "fail forever");
    assert_eq!(obs.reason, StopReason::NoProgress, "failures still halt");
    assert_eq!(
        obs.warnings, 0,
        "the detector Warning/grace never engages on failures"
    );
    assert_eq!(obs.hints, 0, "no model-visible hint on the failure path");
    assert_eq!(
        obs.tool_calls, 12,
        "stops at the consecutive-failure limit with no grace-granted extra call"
    );
}

#[test]
fn explicit_budget_trip_below_soft_start_still_stops_like_the_latch() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("f.txt"), "x\n").unwrap();
    std::fs::write(root.join("g.txt"), "y\n").unwrap();
    std::fs::write(root.join("h.txt"), "z\n").unwrap();

    // The compatibility case the recompute must not break: an EXPLICIT budget
    // (soft 5, max 50) with the trip happening BELOW the soft start. The cost
    // controller continues while calls_used < soft_start, so calls 4 and 5 (novel
    // reads) DO dispatch after the trip and clear the DYNAMIC signal — but the
    // monotone since-reset view the controller reads stays set, so at calls_used
    // == 5 the turn still stops NoProgress before a sixth call, exactly as the old
    // irreversible latch did. (With a naive recoverable `is_tripped`, calls 4/5
    // would clear it and the turn would run on — the regression this pins.)
    let provider = FakeProvider::new()
        .tool_call("c1", "read_file", json!({ "path": "f.txt" }))
        .tool_call("c2", "read_file", json!({ "path": "f.txt" }))
        .tool_call("c3", "read_file", json!({ "path": "f.txt" }))
        .tool_call("c4", "read_file", json!({ "path": "g.txt" }))
        .tool_call("c5", "read_file", json!({ "path": "h.txt" }))
        .tool_call("c6", "read_file", json!({ "path": "f.txt" }))
        .text("done");

    let obs = observe_turn(
        runtime_budgets(root, provider, 5, 50),
        "explicit trip below soft",
    );
    assert_eq!(
        obs.reason,
        StopReason::NoProgress,
        "the explicit controller still stops on no-progress at the soft start"
    );
    assert_eq!(
        obs.tool_calls, 5,
        "5 calls execute (trip below soft, novel calls clear the dynamic signal), \
         then the 6th is stopped — matching the old latch"
    );
}
