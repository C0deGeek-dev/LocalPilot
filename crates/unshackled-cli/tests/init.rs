//! End-to-end test for `unshackled init`.
#![allow(clippy::unwrap_used)]

use assert_cmd::Command;

#[test]
fn init_creates_config_and_gitignore_entry() {
    let dir = tempfile::tempdir().unwrap();
    unshackled_cmd()
        .current_dir(dir.path())
        .arg("init")
        .assert()
        .success();

    assert!(dir.path().join(".unshackled.toml").exists());
    let gitignore = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert!(
        gitignore.lines().any(|l| l.trim() == ".unshackled/"),
        "gitignore missing entry: {gitignore}"
    );
}

#[test]
fn init_is_idempotent_and_preserves_existing_gitignore() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".gitignore"), "/target\n").unwrap();

    for _ in 0..2 {
        unshackled_cmd()
            .current_dir(dir.path())
            .arg("init")
            .assert()
            .success();
    }

    let gitignore = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
    assert!(gitignore.contains("/target"));
    // The entry appears exactly once even after repeated init.
    assert_eq!(
        gitignore
            .lines()
            .filter(|l| l.trim() == ".unshackled/")
            .count(),
        1
    );
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
