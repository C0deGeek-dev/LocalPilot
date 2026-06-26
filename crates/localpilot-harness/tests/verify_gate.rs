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
fn gate_gives_up_after_the_attempt_cap_so_it_never_loops_forever() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let marker = "never_created.txt";
    let command = marker_verify_command(root, marker);

    // A model that keeps claiming completion without ever creating the marker:
    // the verification can never pass. The gate must still terminate the turn
    // (the fixed re-entry cap), not spin forever.
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
        StopReason::Done,
        "the gate finalizes after the re-entry cap rather than looping forever"
    );
    assert!(!root.join(marker).is_file());
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
