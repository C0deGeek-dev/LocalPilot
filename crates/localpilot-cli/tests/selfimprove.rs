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

/// Legacy proposals remain inspectable, but approval alone cannot invent passing
/// evidence. No model/build/reload is used.
#[test]
fn legacy_proposals_remain_inspectable_but_cannot_promote_without_validation() {
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
    assert!(!output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("passing validation"));
    assert!(localpilot_patchgen::ProposedPatch::reopen(repo.path(), "persisted-review").is_ok());
    assert_eq!(
        std::fs::read_to_string(repo.path().join("sample.py")).unwrap(),
        "def answer():\n    return 1\n"
    );
    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
    assert_eq!(after["stage"], "proposed");
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
    for operation in ["preflight", "validate", "evidence"] {
        assert!(stdout.contains(operation), "{stdout}");
    }
}

/// Real binary flow: viewing grants nothing; execution consent produces evidence;
/// a distinct promotion review moves the source and matching loop state together.
#[test]
fn explicit_execution_then_separate_direct_promotion_updates_the_matching_loop() {
    use localpilot_patchgen::{ChangeProvenance, PatchProposal, ProposedEdit};
    let repo = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
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
        assert!(output.status.success(), "{output:?}");
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
    std::fs::create_dir(repo.path().join("src")).unwrap();
    for (name, content) in [
        (".gitignore", ".localpilot/\ntarget/\n".to_owned()),
        (
            "Cargo.toml",
            "[package]\nname='cli_validation_fixture'\nversion='0.0.0'\nedition='2021'\n"
                .to_owned(),
        ),
        (
            "Cargo.lock",
            "version = 3\n[[package]]\nname = \"cli_validation_fixture\"\nversion = \"0.0.0\"\n"
                .to_owned(),
        ),
        (
            "src/lib.rs",
            "#[test] fn answer() { assert_eq!(1, 1); }\n".to_owned(),
        ),
        (
            "rust-toolchain.toml",
            format!("[toolchain]\nchannel='{toolchain}'\n"),
        ),
        (
            ".localpilot.toml",
            "[[harness.checks]]\nname='test'\nprogram='cargo'\nargs=['test','--workspace']\n"
                .to_owned(),
        ),
    ] {
        std::fs::write(repo.path().join(name), content).unwrap();
    }
    git(&["add", "-f", "."]);
    git(&["commit", "-m", "controlled validation fixture"]);
    let before = git(&["rev-parse", "HEAD"]);
    let patch = localpilot_patchgen::propose(
        repo.path(),
        "cli-reviewed",
        &PatchProposal::new(
            "controlled fixture",
            vec!["src/lib.rs".into()],
            vec![ProposedEdit::new(
                "src/lib.rs",
                "#[test] fn answer() { assert_eq!(2, 2); }\n",
            )],
        ),
        ChangeProvenance::new("fixture", "scripted", "explicit two-gate flow"),
    )
    .unwrap();
    patch.persist().unwrap();
    std::fs::create_dir_all(config.path().join("localpilot")).unwrap();
    std::fs::write(
        config.path().join("localpilot/trusted-folders.txt"),
        format!("{}\n", repo.path().canonicalize().unwrap().display()),
    )
    .unwrap();
    let state_path = repo.path().join(".localpilot/selfimprove/state.json");
    std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
    std::fs::write(
        &state_path,
        r#"{"stage":"proposed","proposal_id":"cli-reviewed"}"#,
    )
    .unwrap();
    let run = |args: &[&str]| {
        let output = bin()
            .args(args)
            .env("APPDATA", config.path())
            .env("XDG_CONFIG_HOME", config.path())
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    let preview = run(&["selfimprove", "validate"]);
    assert!(preview.contains("No execution authorized"));
    let execution_review = preview
        .lines()
        .find_map(|line| line.strip_prefix("Preview identity: "))
        .unwrap();
    {
        let mut authority =
            localpilot_patchgen::validation::RepositoryAuthority::open(repo.path()).unwrap();
        assert_eq!(authority.try_transaction().unwrap().events().count(), 0);
    }
    let validation = run(&[
        "selfimprove",
        "validate",
        "--approve-execution",
        "--reviewer",
        "fixture-human",
        "--review",
        execution_review,
    ]);
    assert!(validation.contains("Validation Passed"), "{validation}");
    assert_eq!(git(&["rev-parse", "HEAD"]), before);
    let proposed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    assert_eq!(proposed["stage"], "proposed");
    let evidence = run(&["selfimprove", "evidence"]);
    let promotion_review = evidence
        .lines()
        .find_map(|line| {
            line.strip_prefix(
                "Current passing evidence verified locally. Promotion review identity: ",
            )
        })
        .unwrap();
    assert_ne!(execution_review, promotion_review);
    let promoted = run(&[
        "self-review",
        "promote",
        "--id",
        "cli-reviewed",
        "--approve",
        "--reviewer",
        "fixture-human",
        "--review",
        promotion_review,
    ]);
    assert!(promoted.contains("Promoted"), "{promoted}");
    let approved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    assert_eq!(approved["stage"], "approved");
    assert!(approved.get("built_label").is_none());
    assert_eq!(
        std::fs::read_to_string(repo.path().join("src/lib.rs")).unwrap(),
        "#[test] fn answer() { assert_eq!(2, 2); }\n"
    );
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
