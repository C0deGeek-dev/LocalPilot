//! `localpilot mesh` passes the vendored pair-programming conformance suite in
//! the participant profile, playing `codex` and `localpilot` against the
//! suite's pinned reference implementation.
//!
//! The suite's runner is Python. It is found from
//! `LOCALPILOT_CONFORMANCE_PYTHON`, then `python3`, `python` and `py -3`.
//! Without one the test fails under `CI=true` and is skipped with a notice
//! elsewhere, so a developer machine without Python still runs the rest.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn suite() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("localpilot-mesh")
        .join("conformance")
}

/// A command that runs Python 3.9 or newer, as program plus leading args.
fn python() -> Option<Vec<String>> {
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

#[test]
fn the_participant_passes_every_mandatory_fixture() {
    let Some(py) = python() else {
        let msg = "no Python 3.9+ found (set LOCALPILOT_CONFORMANCE_PYTHON); the mesh conformance suite did not run";
        assert!(std::env::var("CI").as_deref() != Ok("true"), "{msg}");
        eprintln!("NOTICE: {msg}");
        return;
    };
    let exe = env!("CARGO_BIN_EXE_localpilot");
    // The runner splits each participant command on whitespace.
    assert!(
        !exe.chars().any(char::is_whitespace),
        "the test binary path has whitespace, which the runner cannot pass: {exe}"
    );
    let native = format!("{exe} mesh");
    let out = Command::new(&py[0])
        .args(&py[1..])
        .arg(suite().join("run.py"))
        .arg("--participant")
        .arg(format!("codex={native}"))
        .arg("--participant")
        .arg(format!("localpilot={native}"))
        .current_dir(suite())
        .env("PYTHONIOENCODING", "utf-8")
        .output()
        .expect("run the conformance runner");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let report = format!("{stdout}\n{stderr}");
    let failures: Vec<&str> = stdout
        .lines()
        .filter(|l| {
            l.starts_with("FAIL ") || l.starts_with("  ") || l.starts_with("MANDATORY_NOT_RUN")
        })
        .collect();
    assert!(
        out.status.success(),
        "conformance failed:\n{}\n{}",
        failures.join("\n"),
        report.lines().rev().take(5).collect::<Vec<_>>().join("\n")
    );
    let summary = stdout
        .lines()
        .find(|l| l.starts_with("SELECTED "))
        .unwrap_or_default();
    // The expected count comes from the vendored mandatory list, not from the
    // runner's own report, so a runner that selects fewer fixtures fails here.
    let listed: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(suite().join("participant.json")).expect("participant.json"),
    )
    .expect("participant.json is JSON");
    let mandatory = listed["mandatory"]
        .as_array()
        .expect("participant.json lists mandatory fixtures")
        .len();
    assert!(mandatory > 0, "the mandatory list is empty");
    assert_eq!(
        summary,
        format!("SELECTED {mandatory} / TOTAL {mandatory} / SKIPPED 0 / FAILED 0"),
        "every mandatory fixture must run and pass"
    );
    assert!(
        !stdout.contains("MANDATORY_NOT_RUN"),
        "a mandatory fixture did not run:\n{stdout}"
    );
}
