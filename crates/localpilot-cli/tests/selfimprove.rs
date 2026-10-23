//! End-to-end shape of the `localpilot selfimprove` surface: the subcommand is
//! wired to the orchestrator, `status` surfaces the loop state, and `next` stops
//! hard at the human approval gate without promoting.
#![allow(clippy::unwrap_used)]

use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_localpilot"))
}

/// Actual CLI preflight reads the committed base even when the proposed check
/// configuration would execute a shell. No compiler/test/model is launched.
#[test]
fn preflight_shows_the_base_command_and_preserves_the_proposal_without_execution() {
    use localpilot_patchgen::{ChangeProvenance, PatchProposal, ProposedEdit};
    let repo = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .current_dir(repo.path())
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "commit.gpgSign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.name", "fixture"]);
    git(&["config", "user.email", "fixture@localhost"]);
    git(&["config", "core.autocrlf", "false"]);
    let toolchain = std::env::var("RUSTUP_TOOLCHAIN").unwrap_or_else(|_| "1.82".into());
    assert!(toolchain
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_')));
    std::fs::write(repo.path().join(".gitignore"), ".localpilot/\ntarget/\n").unwrap();
    std::fs::write(
        repo.path().join("rust-toolchain.toml"),
        format!("[toolchain]\nchannel='{toolchain}'\n"),
    )
    .unwrap();
    std::fs::write(
        repo.path().join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.0.0'\n",
    )
    .unwrap();
    std::fs::write(
        repo.path().join(".localpilot.toml"),
        "[[harness.checks]]\nname='check'\nprogram='cargo'\nargs=['check','--workspace']\n",
    )
    .unwrap();
    git(&["add", "-f", "."]);
    git(&["commit", "-m", "controlled static preview fixture"]);
    let before = git(&["rev-parse", "HEAD"]);
    let proposal = PatchProposal::new("controlled preview fixture", vec![".localpilot.toml".into()], vec![ProposedEdit::new(".localpilot.toml", "[[harness.checks]]\nname='attack'\nprogram='sh'\nargs=['-c','touch MUST_NOT_EXECUTE']\n")]);
    let patch = localpilot_patchgen::propose(
        repo.path(),
        "preflight-preview",
        &proposal,
        ChangeProvenance::new("fixture", "scripted", "base profile authority"),
    )
    .unwrap();
    let worktree = patch.worktree_path().to_owned();
    patch.persist().unwrap();
    let output = bin()
        .args(["self-review", "preflight", "--id", "preflight-preview"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let shown = String::from_utf8_lossy(&output.stdout);
    assert!(shown.contains("Ordered execute-only plan"), "{shown}");
    assert!(shown.contains("--frozen"), "{shown}");
    assert!(shown.contains("does not contain filesystem"), "{shown}");
    assert!(shown.contains("Risk Configuration"), "{shown}");
    assert!(shown.contains("No evaluation token issued"), "{shown}");
    assert!(!worktree.join("MUST_NOT_EXECUTE").exists());
    assert!(
        !worktree.join("target").exists(),
        "preflight cannot compile the proposal"
    );
    assert!(worktree.is_dir(), "preview leaves the proposal inspectable");
    assert_eq!(git(&["rev-parse", "HEAD"]), before);
    let mut authority =
        localpilot_patchgen::validation::RepositoryAuthority::open(repo.path()).unwrap();
    assert_eq!(authority.try_transaction().unwrap().events().count(), 0);
}

/// Rendering a persisted diff must not drop the resource that the immediately
/// following explicit approval needs to reopen. No model/build/reload is used.
#[test]
fn reviewing_the_persisted_diff_does_not_destroy_the_proposal_before_approval() {
    use localpilot_patchgen::{ChangeProvenance, PatchProposal, ProposedEdit};
    let repo = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let result = Command::new("git")
            .current_dir(repo.path())
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "commit.gpgSign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.name", "fixture"]);
    git(&["config", "user.email", "fixture@localhost"]);
    git(&["config", "core.autocrlf", "false"]);
    git(&["config", "core.eol", "lf"]);
    std::fs::write(repo.path().join(".gitignore"), ".localpilot/\n").unwrap();
    std::fs::write(
        repo.path().join("sample.py"),
        "def answer():\n    return 1\n",
    )
    .unwrap();
    git(&["add", "."]);
    git(&["commit", "-m", "controlled fixture"]);
    let proposal = PatchProposal::new(
        "controlled review fixture",
        vec!["sample.py".into()],
        vec![ProposedEdit::new(
            "sample.py",
            "def answer():\n    return 2\n",
        )],
    );
    localpilot_patchgen::propose(
        repo.path(),
        "persisted-review",
        &proposal,
        ChangeProvenance::new("fixture", "scripted", "review a persisted diff"),
    )
    .unwrap()
    .persist()
    .unwrap();
    let state_dir = repo.path().join(".localpilot/selfimprove");
    std::fs::create_dir_all(&state_dir).unwrap();
    let state = state_dir.join("state.json");
    std::fs::write(
        &state,
        r#"{"stage":"proposed","proposal_id":"persisted-review"}"#,
    )
    .unwrap();
    let output = bin()
        .args([
            "selfimprove",
            "next",
            "--approve",
            "--reviewer",
            "fixture-human",
        ])
        .env("APPDATA", config.path())
        .env("XDG_CONFIG_HOME", config.path())
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let shown = String::from_utf8_lossy(&output.stdout);
    assert!(shown.contains("+    return 2"), "{shown}");
    assert!(shown.contains("Promoted"), "{shown}");
    assert_eq!(
        std::fs::read_to_string(repo.path().join("sample.py")).unwrap(),
        "def answer():\n    return 2\n"
    );
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
    assert_eq!(after["stage"], "approved");
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
