//! Smoke tests for the installed binary shape.
#![allow(clippy::unwrap_used)]

use assert_cmd::Command;

#[test]
#[cfg(feature = "tui")]
fn tui_build_prints_top_level_help() {
    let output = Command::new("cargo")
        .args([
            "run",
            "--quiet",
            "--manifest-path",
            concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
            "--features",
            "tui",
            "--",
            "--help",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("LocalMind learning: closeout, review queue, memory"));
    assert!(stdout.contains("Launch the interactive terminal REPL"));
}
