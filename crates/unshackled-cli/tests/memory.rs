//! End-to-end tests for `unshackled memory`.
#![allow(clippy::unwrap_used)]

use assert_cmd::Command;
use unshackled_core::{Message, Role, SessionId};
use unshackled_localmind::ReviewVerdict;
use unshackled_store::Store;

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
    let id = promoted_memory(dir.path(), "the parser handles errors");

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
    assert!(unshackled_localmind::memory_list(dir.path())
        .unwrap()
        .is_empty());

    // Disable stops injection.
    run(dir.path(), &["memory", "disable"]);
    assert!(!unshackled_localmind::memory_injection_enabled(dir.path()));
    assert!(unshackled_localmind::context_for(dir.path(), "parser")
        .unwrap()
        .is_none());
}

fn promoted_memory(dir: &std::path::Path, lesson: &str) -> String {
    let store = Store::open(dir);
    let session = SessionId::new();
    store
        .append_message(
            session,
            &Message::text(Role::User, format!("Lesson: {lesson}")),
        )
        .unwrap();
    unshackled_localmind::closeout_session(dir, &store, session).unwrap();
    let item = unshackled_localmind::review_list(dir)
        .unwrap()
        .into_iter()
        .find(|item| item.summary == lesson)
        .unwrap();
    unshackled_localmind::review_decide(dir, &item.id, ReviewVerdict::Accept, "test", None)
        .unwrap();
    unshackled_localmind::promote(dir, &item.id).unwrap()
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
