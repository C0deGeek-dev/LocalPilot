//! The verify-before-done gate re-runs a build/test verification when a turn
//! would finalize, feeding a failure back into the loop instead of accepting
//! code that never passed. With the gate off the turn finalizes immediately.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::Arc;

use localpilot_harness::{SessionConfig, SessionRuntime, StopReason};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use serde_json::json;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// A verification command that fails until `marker` exists in the workspace
/// root, then passes — expressed as a whitespace-splittable command line (no
/// shell quoting), per tier-1 platform. An absolute marker path is used so the
/// command does not depend on the verify runner's working directory (the
/// canonicalized workspace root is a Windows `\\?\` verbatim path that the
/// `cmd.exe` builtins mishandle). `cmd /C type` exits non-zero when the file is
/// absent; `test -f` does the same on Unix.
fn marker_verify_command(root: &Path, marker: &str) -> String {
    let abs = root.join(marker);
    let abs = abs.to_string_lossy().into_owned();
    // The test workspace lives under the OS temp dir (no spaces), so a
    // whitespace split keeps the path one argument.
    assert!(
        !abs.contains(' '),
        "test temp path unexpectedly has a space: {abs}"
    );
    #[cfg(windows)]
    {
        format!("cmd /C type {abs}")
    }
    #[cfg(not(windows))]
    {
        format!("test -f {abs}")
    }
}

/// A verification command that checks for `marker` by its **relative** name, so
/// it passes only when run in the workspace (the gate's de-verbatim cwd). Used to
/// prove the gate runs in-workspace, not a fallback directory.
fn relative_marker_verify_command(marker: &str) -> String {
    #[cfg(windows)]
    {
        format!("cmd /C type {marker}")
    }
    #[cfg(not(windows))]
    {
        format!("test -f {marker}")
    }
}

/// A verification command that always passes (exit 0) on each tier-1 platform.
fn always_pass_command() -> String {
    #[cfg(windows)]
    {
        "cmd /C exit 0".to_string()
    }
    #[cfg(not(windows))]
    {
        "true".to_string()
    }
}

fn runtime_with_verify(
    root: &Path,
    provider: FakeProvider,
    verify_command: Option<String>,
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
            verify_before_done: verify_command.is_some(),
            verify_command,
            ..SessionConfig::default()
        },
        Vec::new(),
    )
}

/// A model that first claims completion (a call-free turn) without doing the
/// work, then — once the gate feeds the failure back — writes the marker file
/// and claims completion again.
fn fixes_on_second_chance(marker: &str) -> FakeProvider {
    FakeProvider::new()
        .text("All set.")
        .tool_call(
            "w1",
            "write_file",
            json!({ "path": marker, "content": "fixed\n" }),
        )
        .text("Now it is done.")
}

#[test]
fn gate_on_re_enters_until_verification_passes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let marker = "fixed.txt";
    let command = marker_verify_command(root, marker);

    let mut runtime = runtime_with_verify(root, fixes_on_second_chance(marker), Some(command));
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn("solve it", &events, &cancel));

    assert_eq!(reason, StopReason::Done, "the turn finalizes once green");
    assert!(
        root.join(marker).is_file(),
        "with the gate on, the failing verification was fed back and the fix was applied"
    );
}

#[test]
fn gate_off_finalizes_without_running_verification() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let marker = "fixed.txt";
    // Write the shim path determinism is irrelevant here — the gate is off.
    let _ = marker_verify_command(root, marker);

    // Same model, but the gate is off (no verify command) — the first call-free
    // turn finalizes immediately and the marker is never written.
    let mut runtime = runtime_with_verify(root, fixes_on_second_chance(marker), None);
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn("solve it", &events, &cancel));

    assert_eq!(reason, StopReason::Done);
    assert!(
        !root.join(marker).is_file(),
        "with the gate off, the turn finalizes on the first call-free reply — no fix forced"
    );
}

#[test]
fn gate_gives_up_with_no_progress_after_the_attempt_cap() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let marker = "never_created.txt";
    let command = marker_verify_command(root, marker);

    // A model that keeps claiming completion without ever creating the marker:
    // the verification can never pass. The gate must terminate the turn after the
    // fixed re-entry cap (never spin forever) and report `NoProgress` — the
    // verify signal driving the no-progress stop — not a clean `Done`.
    let provider = FakeProvider::new()
        .text("done")
        .text("done")
        .text("done")
        .text("done")
        .text("done")
        .text("done");

    let mut runtime = runtime_with_verify(root, provider, Some(command));
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn("solve it", &events, &cancel));

    assert_eq!(
        reason,
        StopReason::NoProgress,
        "a build that never goes green stops with NoProgress after the re-entry cap"
    );
    assert!(!root.join(marker).is_file());
}

#[test]
fn gate_runs_the_verification_in_the_workspace_not_a_fallback_dir() {
    // End-to-end cwd regression: the gate's verify command runs in the
    // workspace's de-verbatim cwd, so a *relative* marker check resolves against
    // the workspace. Before the fix the gate ran in a verbatim `\\?\` cwd that
    // `cmd` mishandled (falling back to `C:\Windows`), so a relative check could
    // never pass. The model writes the marker by its relative name; the gate sees
    // it only because it is running in the workspace.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let marker = "verified_in_workspace.txt";
    let command = relative_marker_verify_command(marker);

    let mut runtime = runtime_with_verify(root, fixes_on_second_chance(marker), Some(command));
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn("solve it", &events, &cancel));

    assert_eq!(
        reason,
        StopReason::Done,
        "a relative-path verification passed — the gate ran in the workspace"
    );
    assert!(root.join(marker).is_file());
}

#[test]
fn gate_on_a_green_workspace_finalizes_done_without_re_entry() {
    // With the gate on, a workspace whose verification already
    // passes finalizes `Done` on the first call-free reply. The gate only *adds* a
    // build/test step on finalize — it can never unpass a passing solve. The model
    // carries no fix tool, so a spurious re-entry would change the outcome.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let provider = FakeProvider::new().text("done");

    let mut runtime = runtime_with_verify(root, provider, Some(always_pass_command()));
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn("solve it", &events, &cancel));

    assert_eq!(
        reason,
        StopReason::Done,
        "a green verification finalizes Done unchanged — the gate adds a step, it does not unpass"
    );
}

#[test]
fn the_eval_setter_enables_the_gate_at_runtime() {
    // `eval --verify` flips the gate on after `build_runtime` via this setter, so
    // a benchmark arm can enable it without a config file. Same run_turn path as
    // the config route, so the re-entry behaviour above carries over.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let marker = "fixed.txt";
    let command = marker_verify_command(root, marker);

    let mut runtime = runtime_with_verify(root, fixes_on_second_chance(marker), None);
    runtime.set_verify_before_done(true, Some(command));
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn("solve it", &events, &cancel));

    assert_eq!(reason, StopReason::Done);
    assert!(
        root.join(marker).is_file(),
        "the runtime-enabled gate forced the fix, like the config-enabled gate"
    );
}

// ---- `[harness] verification` = advisory | manual (LocalHub#267) ----

fn check_statuses(runtime: &SessionRuntime) -> Vec<String> {
    runtime
        .store()
        .read_events(runtime.session_id())
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.kind {
            localpilot_store::SessionEventKind::CheckRan { status, .. } => Some(status),
            _ => None,
        })
        .collect()
}

fn run_with_policy(
    policy: localpilot_config::VerificationPolicy,
) -> (SessionRuntime, StopReason, tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let marker = "fixed.txt";
    let command = marker_verify_command(&root, marker);
    let mut runtime = runtime_with_verify(&root, fixes_on_second_chance(marker), Some(command));
    runtime.set_verification_policy(policy, false);
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn("solve it", &events, &cancel));
    (runtime, reason, dir, marker.to_string())
}

#[test]
fn advisory_runs_the_check_records_the_failure_and_requests_no_repair() {
    let (runtime, reason, dir, marker) =
        run_with_policy(localpilot_config::VerificationPolicy::Advisory);

    assert_eq!(reason, StopReason::Done, "the answer is kept");
    assert!(
        !dir.path().join(marker).exists(),
        "no repair turn was requested"
    );
    assert_eq!(check_statuses(&runtime), ["failed"]);
}

#[test]
fn manual_runs_no_check_and_records_that_it_did_not() {
    let (runtime, reason, dir, marker) =
        run_with_policy(localpilot_config::VerificationPolicy::Manual);

    assert_eq!(reason, StopReason::Done);
    assert!(!dir.path().join(marker).exists());
    assert_eq!(check_statuses(&runtime), ["not_run"]);
}

#[test]
fn current_still_feeds_the_failure_back() {
    let (runtime, reason, dir, marker) =
        run_with_policy(localpilot_config::VerificationPolicy::Current);

    assert_eq!(reason, StopReason::Done);
    assert!(dir.path().join(marker).is_file(), "the repair turn ran");
    assert!(check_statuses(&runtime).iter().any(|s| s == "failed"));
}

/// A command that outlives the 300 s check timeout. Slow, so it runs on request:
/// `cargo test -p localpilot-harness --test verify_gate -- --ignored hanging`.
fn hanging_command() -> String {
    #[cfg(windows)]
    {
        "ping -n 900 127.0.0.1".to_string()
    }
    #[cfg(not(windows))]
    {
        "sleep 900".to_string()
    }
}

#[test]
#[ignore = "waits for the 300 s check timeout"]
fn hanging_check_under_each_policy() {
    for policy in [
        localpilot_config::VerificationPolicy::Advisory,
        localpilot_config::VerificationPolicy::Current,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut runtime = runtime_with_verify(
            dir.path(),
            FakeProvider::new().text("All set.").text("Still done."),
            Some(hanging_command()),
        );
        runtime.set_verification_policy(policy, false);
        let (events, _rx) = broadcast::channel(64);
        let cancel = CancellationToken::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let started = std::time::Instant::now();
        let reason = rt.block_on(runtime.run_turn("solve it", &events, &cancel));
        eprintln!(
            "HANGING {policy:?}: stop={reason:?} after {:?}, checks={:?}",
            started.elapsed(),
            check_statuses(&runtime)
        );
        // A timeout is an errored check, never a pass, and the read-only turn ends.
        assert_eq!(reason, StopReason::Done);
        assert_eq!(check_statuses(&runtime), ["errored"]);
    }
}

/// Required verification (the turn wrote a file) with a verifier that cannot run.
fn required_but_unrunnable(
    policy: localpilot_config::VerificationPolicy,
) -> (SessionRuntime, StopReason, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let mut runtime = SessionRuntime::new(
        Arc::new(
            FakeProvider::new()
                .tool_call(
                    "w1",
                    "write_file",
                    json!({ "path": "out.txt", "content": "changed\n" }),
                )
                .text("done"),
        ),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Bypass, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(&root),
        Workspace::new(&root).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            interactivity: Interactivity::NonInteractive,
            trusted: true,
            require_mutation_verification: Some(true),
            verify_command: Some("definitely-not-a-program-267".to_string()),
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    runtime.set_verification_policy(policy, false);
    let (events, _rx) = broadcast::channel(64);
    let cancel = CancellationToken::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let reason = rt.block_on(runtime.run_turn("write it", &events, &cancel));
    (runtime, reason, dir)
}

#[test]
fn a_verifier_that_cannot_run_blocks_required_work_only_under_current() {
    use localpilot_config::VerificationPolicy::{Advisory, Current, Manual};
    let (current, stop, _d1) = required_but_unrunnable(Current);
    assert_eq!(stop, StopReason::NoProgress, "current stops, unverified");
    assert!(!check_statuses(&current).iter().any(|s| s == "passed"));

    let (advisory, stop, _d2) = required_but_unrunnable(Advisory);
    assert_eq!(stop, StopReason::Done, "advisory records it and finishes");
    let statuses = check_statuses(&advisory);
    assert!(
        !statuses.is_empty() && !statuses.iter().any(|s| s == "passed"),
        "{statuses:?}"
    );

    let (manual, stop, _d3) = required_but_unrunnable(Manual);
    assert_eq!(stop, StopReason::Done);
    assert_eq!(check_statuses(&manual), ["not_run"]);
}
