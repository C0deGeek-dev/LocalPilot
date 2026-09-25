//! Shared by the tests that run the vendored pair-programming conformance
//! suite: where it is, how to run its Python tools, and the native command.
//! Each test binary uses a different subset.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// The vendored suite.
pub fn suite() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("localpilot-mesh")
        .join("conformance")
}

/// A command that runs Python 3.9 or newer, as program plus leading args:
/// `LOCALPILOT_CONFORMANCE_PYTHON`, then `python3`, `python` and `py -3`.
pub fn python() -> Option<Vec<String>> {
    let mut candidates: Vec<Vec<String>> = Vec::new();
    if let Ok(p) = std::env::var("LOCALPILOT_CONFORMANCE_PYTHON") {
        if !p.trim().is_empty() {
            candidates.push(vec![p]);
        }
    }
    candidates.push(vec!["python3".into()]);
    candidates.push(vec!["python".into()]);
    candidates.push(vec!["py".into(), "-3".into()]);
    candidates.into_iter().find(|c| {
        Command::new(&c[0])
            .args(&c[1..])
            .args(["-c", "import sys; sys.exit(sys.version_info < (3, 9))"])
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

/// Python, or `None` after a notice; without one the run fails under
/// `CI=true`, so a developer machine without Python still runs the rest.
pub fn python_or_skip(what: &str) -> Option<Vec<String>> {
    let py = python();
    if py.is_none() {
        let msg =
            format!("no Python 3.9+ found (set LOCALPILOT_CONFORMANCE_PYTHON); {what} did not run");
        assert!(std::env::var("CI").as_deref() != Ok("true"), "{msg}");
        eprintln!("NOTICE: {msg}");
    }
    py
}

/// `localpilot mesh`, as the suite's tools take a command: one string they
/// split on whitespace.
pub fn native() -> String {
    let exe = env!("CARGO_BIN_EXE_localpilot");
    assert!(
        !exe.chars().any(char::is_whitespace),
        "the test binary path has whitespace, which the suite's tools cannot pass: {exe}"
    );
    format!("{exe} mesh")
}

/// A Python tool of the suite, ready to take its arguments.
pub fn tool(py: &[String], script: &str) -> Command {
    let mut c = Command::new(&py[0]);
    c.args(&py[1..])
        .arg(suite().join(script))
        .current_dir(suite())
        .env("PYTHONIOENCODING", "utf-8")
        // No `__pycache__` in the vendored copy: its manifest would call it drift.
        .env("PYTHONDONTWRITEBYTECODE", "1");
    c
}
