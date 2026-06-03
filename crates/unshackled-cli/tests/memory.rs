//! End-to-end tests for `unshackled memory`.
#![allow(clippy::unwrap_used)]

use assert_cmd::Command;
use unshackled_memory::{MemoryKind, MemoryStore};

fn run(dir: &std::path::Path, args: &[&str]) -> String {
    let output = unshackled_cmd()
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?} failed: {output:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn memory_inspect_delete_and_disable() {
    let dir = tempfile::tempdir().unwrap();
    let store = MemoryStore::open(dir.path());
    let id = store
        .add(
            MemoryKind::ProjectFact,
            "the parser handles errors",
            vec![],
            true,
        )
        .unwrap();

    // Inspect lists the entry.
    let listed = run(dir.path(), &["memory", "inspect"]);
    assert!(listed.contains(&id));
    assert!(listed.contains("the parser handles errors"));

    // Status reports one entry, enabled.
    let status = run(dir.path(), &["memory", "status"]);
    assert!(status.contains("1 entries"));
    assert!(status.contains("enabled"));

    // Delete removes it.
    let deleted = run(dir.path(), &["memory", "delete", &id]);
    assert!(deleted.contains("deleted"));
    assert!(store.all().unwrap().is_empty());

    // Disable stops injection.
    run(dir.path(), &["memory", "disable"]);
    assert!(!store.is_enabled());
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
