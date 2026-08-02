//! End-to-end shape of the `localpilot selfimprove` surface: the subcommand is
//! wired to the orchestrator, `status` surfaces the loop state, and `next` stops
//! hard at the human approval gate without promoting.
#![allow(clippy::unwrap_used)]

use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_localpilot"))
}

/// `selfimprove --help` lists the wired `status` and `next` subcommands — the
/// surface is real, not dead.
#[test]
fn help_lists_status_and_next() {
    let output = bin().args(["selfimprove", "--help"]).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("status"),
        "help must list `status`: {stdout}"
    );
    assert!(stdout.contains("next"), "help must list `next`: {stdout}");
}

/// On a repo with a finding but no active loop, `status` surfaces the `Found`
/// stage by counting findings through the orchestrator's read-only review.
#[test]
fn status_surfaces_findings_when_idle() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(
        repo.path().join("worker.rs"),
        "pub fn run() {}\n// TODO: handle retries\n",
    )
    .unwrap();

    let output = bin()
        .args(["selfimprove", "status"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("idle"),
        "status should report idle: {stdout}"
    );
    assert!(
        stdout.contains("finding(s)"),
        "status should surface the finding count: {stdout}"
    );
}

/// With the loop parked at the gate, `status` shows it is awaiting approval, and
/// `next` (without `--approve`) prints the required human action and refuses to
/// advance — the structural gate, observed end-to-end.
#[test]
fn next_stops_at_the_gate_without_promoting() {
    let repo = tempfile::tempdir().unwrap();
    let state_dir = repo.path().join(".localpilot").join("selfimprove");
    std::fs::create_dir_all(&state_dir).unwrap();
    let state_path = state_dir.join("state.json");
    // Seed the loop at the gate (a proposal awaiting approval), so the gate-stop is
    // exercised without a live model authoring a real edit.
    std::fs::write(
        &state_path,
        r#"{"stage":"proposed","proposal_id":"self-improve-1"}"#,
    )
    .unwrap();

    let status = bin()
        .args(["selfimprove", "status"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(status.status.success(), "{status:?}");
    let status_out = String::from_utf8_lossy(&status.stdout);
    assert!(
        status_out.contains("PROPOSED") && status_out.contains("awaiting human approval"),
        "status must show the gate is pending: {status_out}"
    );

    let next = bin()
        .args(["selfimprove", "next"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(next.status.success(), "{next:?}");
    let next_out = String::from_utf8_lossy(&next.stdout);
    assert!(
        next_out.contains("Awaiting human approval") && next_out.contains("--approve"),
        "next at the gate must print the required human action: {next_out}"
    );

    // The loop did not advance: it is still parked at the gate, un-promoted.
    let after = std::fs::read_to_string(&state_path).unwrap();
    assert!(
        after.contains("proposed"),
        "next must not advance past the gate without approval: {after}"
    );
}
