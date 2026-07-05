//! End-to-end test for `localpilot export`.

use assert_cmd::Command;
use localpilot_core::{ContentBlock, Message, Role, SessionId};
use localpilot_store::Store;

#[test]
fn export_writes_a_redacted_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path());
    let session = SessionId::new();
    let secret = "sk-abcdefghijklmnopqrstuvwxyz0123";
    store
        .append_message(
            session,
            &Message::new(
                Role::User,
                vec![ContentBlock::text(format!("key={secret}"))],
            ),
        )
        .unwrap();

    let out = dir.path().join("bundle.json");
    localpilot_cmd()
        .current_dir(dir.path())
        .args([
            "export",
            "--session",
            &session.to_string(),
            "--out",
            out.to_str().unwrap(),
        ])
        .assert()
        .success();

    let content = std::fs::read_to_string(&out).unwrap();
    assert!(
        !content.contains(secret),
        "secret leaked into export bundle"
    );
    assert!(content.contains("[REDACTED]"));
}

#[test]
fn export_rejects_an_invalid_session_id() {
    let dir = tempfile::tempdir().unwrap();
    localpilot_cmd()
        .current_dir(dir.path())
        .args([
            "export",
            "--session",
            "not-a-uuid",
            "--out",
            dir.path().join("bundle.json").to_str().unwrap(),
        ])
        .assert()
        .failure();
}

fn localpilot_cmd() -> Command {
    // The prebuilt test binary — never `cargo run` inside a test: nested
    // cargo fights the build-dir lock under nextest (a hang on Linux, an
    // exe-in-use failure on Windows) and re-resolves features.
    Command::new(env!("CARGO_BIN_EXE_localpilot"))
}
