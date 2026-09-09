//! End-to-end tests for `localpilot harness feature` and `harness adopt`
//! (offline, no provider).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use assert_cmd::Command;

const BRIEF: &str = "# Brief: thing\n\n## Summary\n\nDo the thing.\n\n\
## Requirements\n\n- It works\n\n## Constraints\n\n- Be small\n\n\
## Non-Goals\n\n- World peace\n\n## Acceptance Criteria\n\n- A test passes\n";

/// A plan with no `Brief:` header: what every project written before plans
/// recorded their brief revision has on disk.
const PROGRESS: &str = "# Progress: thing\nBranch: feature/thing\n\n## Steps\n\n\
- [x] 1. Write a failing test\n  - commit: abc1234\n  - attempts: 1\n\
- [ ] 2. Implement it\n";

fn project(root: &Path) {
    std::fs::write(root.join("brief.md"), BRIEF).unwrap();
    std::fs::write(root.join("PROGRESS.md"), PROGRESS).unwrap();
}

#[test]
fn feature_appends_without_renumbering_completed_steps() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());

    // A legacy plan is adopted once, deliberately, before anything acts on it.
    localpilot_cmd()
        .current_dir(dir.path())
        .args(["harness", "adopt"])
        .assert()
        .success();

    localpilot_cmd()
        .current_dir(dir.path())
        .args(["harness", "feature", "add a config flag"])
        .assert()
        .success();

    let brief = std::fs::read_to_string(dir.path().join("brief.md")).unwrap();
    assert!(brief.contains("add a config flag"));

    let progress = std::fs::read_to_string(dir.path().join("PROGRESS.md")).unwrap();
    // The completed step keeps its number, commit, and attempts.
    assert!(progress.contains("- [x] 1. Write a failing test"));
    assert!(progress.contains("commit: abc1234"));
    // The new step is appended as number 3.
    assert!(progress.contains("- [ ] 3. Implement: add a config flag"));
}

#[test]
fn feature_rebinds_the_plan_to_the_brief_it_just_changed() {
    // `feature` is the one atomic brief-and-plan migration: it edits both
    // documents together. If it left the old binding in place, the very next
    // command would call the plan stale — for a change this command just made
    // on purpose.
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    localpilot_cmd()
        .current_dir(dir.path())
        .args(["harness", "adopt"])
        .assert()
        .success();

    let before = binding(dir.path());
    localpilot_cmd()
        .current_dir(dir.path())
        .args(["harness", "feature", "add a config flag"])
        .assert()
        .success();
    let after = binding(dir.path());

    assert_ne!(
        before, after,
        "the brief changed, so the recorded revision must change with it"
    );
    let status = localpilot_cmd()
        .current_dir(dir.path())
        .args(["harness", "status"])
        .output()
        .unwrap();
    let text = String::from_utf8(status.stdout).unwrap();
    assert!(
        text.contains("lifecycle: plan current"),
        "the plan is current straight after the migration: {text}"
    );
}

#[test]
fn feature_refuses_an_unadopted_legacy_plan_and_says_what_to_run() {
    // Appending to a plan whose relationship to the brief is unknown would
    // quietly turn "unknown" into "current". The refusal names the remedy.
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());

    let output = localpilot_cmd()
        .current_dir(dir.path())
        .args(["harness", "feature", "add a config flag"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("harness adopt"), "{stderr}");

    // Nothing was written on the way to refusing.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("brief.md")).unwrap(),
        BRIEF
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("PROGRESS.md")).unwrap(),
        PROGRESS
    );
}

#[test]
fn adopt_reports_the_binding_and_leaves_the_brief_alone() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());

    let output = localpilot_cmd()
        .current_dir(dir.path())
        .args(["harness", "adopt"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    // The reported value is the one written to disk: a versioned tag and the
    // full digest, so a later build can tell whether it can compare it at all.
    assert!(stdout.contains("sha256-v1:"), "{stdout}");
    let digest = binding(dir.path());
    assert_eq!(digest.len(), "sha256-v1:".len() + 64, "{digest}");

    assert_eq!(
        std::fs::read_to_string(dir.path().join("brief.md")).unwrap(),
        BRIEF,
        "adoption binds the plan; it does not touch the brief"
    );
}

#[test]
fn status_names_the_unbound_state_instead_of_reporting_no_progress() {
    // The old status collapsed missing, malformed, and unbound into the same
    // uninformative output. Each now says which one it is.
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let text = status_text(dir.path());
    assert!(text.contains("not bound"), "{text}");
    assert!(text.contains("1/2 steps complete"), "{text}");

    std::fs::write(dir.path().join("PROGRESS.md"), "# Progress: thing\n").unwrap();
    let text = status_text(dir.path());
    assert!(text.contains("malformed"), "{text}");

    std::fs::remove_file(dir.path().join("PROGRESS.md")).unwrap();
    let text = status_text(dir.path());
    assert!(text.contains("brief only"), "{text}");
}

fn status_text(root: &Path) -> String {
    let output = localpilot_cmd()
        .current_dir(root)
        .args(["harness", "status"])
        .output()
        .unwrap();
    String::from_utf8(output.stdout).unwrap()
}

fn binding(root: &Path) -> String {
    std::fs::read_to_string(root.join("PROGRESS.md"))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("Brief:").map(|s| s.trim().to_string()))
        .expect("a bound plan records its brief revision")
}

fn localpilot_cmd() -> Command {
    // The prebuilt test binary — never `cargo run` inside a test: nested
    // cargo fights the build-dir lock under nextest (a hang on Linux, an
    // exe-in-use failure on Windows) and re-resolves features.
    Command::new(env!("CARGO_BIN_EXE_localpilot"))
}
