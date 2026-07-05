//! Smoke tests for the installed binary shape.
#![allow(clippy::unwrap_used)]

#[cfg(feature = "tui")]
use assert_cmd::Command;

#[test]
#[cfg(feature = "tui")]
fn tui_build_prints_top_level_help() {
    // The test only compiles with the tui feature, so the prebuilt test
    // binary already carries it — never `cargo run` inside a test (nested
    // cargo fights the build-dir lock under nextest).
    let output = Command::new(env!("CARGO_BIN_EXE_localpilot"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("LocalMind learning: closeout, review queue, memory"));
    assert!(stdout.contains("Launch the interactive terminal REPL"));
}
