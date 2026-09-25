//! `localpilot mesh` passes the vendored pair-programming conformance suite in
//! the participant profile, playing `codex` and `localpilot` against the
//! suite's pinned reference implementation.
//!
//! The suite's runner is Python. It is found from
//! `LOCALPILOT_CONFORMANCE_PYTHON`, then `python3`, `python` and `py -3`.
//! Without one the test fails under `CI=true` and is skipped with a notice
//! elsewhere, so a developer machine without Python still runs the rest.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use support::{native, python_or_skip, suite, tool};

#[test]
fn the_participant_passes_every_mandatory_fixture() {
    let Some(py) = python_or_skip("the mesh conformance suite") else {
        return;
    };
    let native = native();
    let out = tool(&py, "run.py")
        .arg("--participant")
        .arg(format!("codex={native}"))
        .arg("--participant")
        .arg(format!("localpilot={native}"))
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
