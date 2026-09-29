//! The pair-seat evaluation's fixtures stay frozen and still tell a good
//! change from a bad one (`seat-eval/drive.py check`), and its driver is safe
//! to run (`seat-eval/test_drive.py`). The evaluation itself
//! needs a model and is run by hand; this keeps its fixtures trustworthy.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::Path;
use std::process::Command;

use support::python_or_skip;

#[test]
fn the_seat_eval_fixtures_are_frozen_and_discriminate() {
    let Some(py) = python_or_skip("the seat-eval fixture check") else {
        return;
    };
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("localpilot-mesh")
        .join("seat-eval");
    let out = Command::new(&py[0])
        .args(&py[1..])
        .arg(dir.join("drive.py"))
        .arg("check")
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success() && text.contains("CHECK OK"), "{text}");
}

#[test]
fn the_seat_eval_driver_deletes_only_its_own_runs_and_never_leaves_an_engine() {
    let Some(py) = python_or_skip("the seat-eval driver tests") else {
        return;
    };
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("localpilot-mesh")
        .join("seat-eval");
    let out = Command::new(&py[0])
        .args(&py[1..])
        .args(["-m", "unittest", "test_drive"])
        .current_dir(&dir)
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success() && text.contains("OK"), "{text}");
}
