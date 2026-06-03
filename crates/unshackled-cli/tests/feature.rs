//! End-to-end test for `unshackled harness feature` (offline, no provider).
#![allow(clippy::unwrap_used)]

use assert_cmd::Command;

const BRIEF: &str = "# Brief: thing\n\n## Summary\n\nDo the thing.\n\n\
## Requirements\n\n- It works\n\n## Constraints\n\n- Be small\n\n\
## Non-Goals\n\n- World peace\n\n## Acceptance Criteria\n\n- A test passes\n";

const PROGRESS: &str = "# Progress: thing\nBranch: feature/thing\n\n## Steps\n\n\
- [x] 1. Write a failing test\n  - commit: abc1234\n  - attempts: 1\n\
- [ ] 2. Implement it\n";

#[test]
fn feature_appends_without_renumbering_completed_steps() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("brief.md"), BRIEF).unwrap();
    std::fs::write(dir.path().join("PROGRESS.md"), PROGRESS).unwrap();

    unshackled_cmd()
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

fn unshackled_cmd() -> Command {
    let mut command = Command::new("cargo");
    command.args([
        "run",
        "--quiet",
        "--manifest-path",
        concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
        "--",
    ]);
    command
}
