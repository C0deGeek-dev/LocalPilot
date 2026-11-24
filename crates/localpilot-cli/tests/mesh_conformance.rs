//! `localpilot mesh` passes the vendored pair-programming conformance suite in
//! the participant profile, playing `codex` and `localpilot` against the
//! suite's pinned reference implementation.
//!
//! The suite's runner is Python. It is found from
//! `LOCALPILOT_CONFORMANCE_PYTHON`, then `python3`, `python` and `py -3`.
//! Without one the test fails under `CI=true` and is skipped with a notice
//! elsewhere, so a developer machine without Python still runs the rest.
//!
//! The runner discards what a fixture's parallel commands print, so a command
//! that fails there leaves only its exit code. The reference implementation
//! therefore runs through `support/record_failures.py`, in the runner's own
//! Python process, which keeps a failing command's exit code and error text;
//! the assertion prints them. `localpilot mesh` is not wrapped: a second
//! process per command pushed the suite past the per-test ceiling on Windows.
//!
//! The mandatory list runs in eight shards, one test each, of about 40 s
//! alone, so none nears the five-minute ceiling under a full parallel run;
//! `.config/nextest.toml` lets at most two run at a time.
//! The shards partition the list by position; a separate test checks that
//! the vendored fixtures are exactly the mandatory list, so none can fall
//! between shards.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::Path;

use support::{native, python_or_skip, suite, tool};

/// What every failing command said, oldest first, for the failure message.
fn failed_commands(dir: &Path) -> String {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map(|entries| entries.filter_map(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    let texts: Vec<String> = files
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .map(|text| text.chars().take(300).collect())
        .collect();
    if texts.is_empty() {
        return "no command recorded a failure".to_string();
    }
    format!(
        "{} failing command(s), refusals a fixture expects included:\n{}",
        texts.len(),
        texts.join("\n")
    )
}

/// How many shards the mandatory list is run in.
const SHARDS: usize = 8;

/// The vendored mandatory list, in its own order.
fn mandatory() -> Vec<String> {
    let listed: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(suite().join("participant.json")).expect("participant.json"),
    )
    .expect("participant.json is JSON");
    let ids: Vec<String> = listed["mandatory"]
        .as_array()
        .expect("participant.json lists mandatory fixtures")
        .iter()
        .map(|v| v.as_str().expect("fixture ids are strings").to_owned())
        .collect();
    assert!(!ids.is_empty(), "the mandatory list is empty");
    ids
}

/// Every fixture the vendored suite ships, by id.
fn on_disk() -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(suite().join("fixtures"))
        .expect("the vendored fixtures")
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".json").map(str::to_owned)
        })
        .collect();
    ids.sort();
    ids
}

#[test]
fn every_mandatory_fixture_is_vendored() {
    let on_disk = on_disk();
    let missing: Vec<String> = mandatory()
        .into_iter()
        .filter(|id| !on_disk.contains(id))
        .collect();
    assert!(
        missing.is_empty(),
        "mandatory but not vendored: {missing:?}"
    );
}

/// The vendored runner owns participant selection. Every fixture that is
/// not on the mandatory list must be one it skips for this participant, so
/// no fixture this participant should pass falls outside the shards.
#[test]
fn every_other_fixture_is_one_the_runner_skips() {
    let Some(py) = python_or_skip("the non-mandatory fixtures") else {
        return;
    };
    let listed = mandatory();
    let others: Vec<String> = on_disk()
        .into_iter()
        .filter(|id| !listed.contains(id))
        .collect();
    if others.is_empty() {
        return;
    }
    let native = native();
    let out = tool(&py, "run.py")
        .arg("--participant")
        .arg(format!("codex={native}"))
        .arg("--participant")
        .arg(format!("localpilot={native}"))
        .args(others.iter().map(|id| {
            let p = suite().join("fixtures").join(format!("{id}.json"));
            dunce::simplified(&p).display().to_string()
        }))
        .output()
        .expect("run the conformance runner");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let n = others.len();
    assert!(
        out.status.success(),
        "the runner failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains(&format!("SELECTED 0 / TOTAL {n} / SKIPPED {n} / FAILED 0")),
        "a non-mandatory fixture was selected for this participant:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn shard_1_of_8() {
    run_shard(0);
}

#[test]
fn shard_2_of_8() {
    run_shard(1);
}

#[test]
fn shard_3_of_8() {
    run_shard(2);
}

#[test]
fn shard_4_of_8() {
    run_shard(3);
}

#[test]
fn shard_5_of_8() {
    run_shard(4);
}

#[test]
fn shard_6_of_8() {
    run_shard(5);
}

#[test]
fn shard_7_of_8() {
    run_shard(6);
}

#[test]
fn shard_8_of_8() {
    run_shard(7);
}

/// Run every mandatory fixture whose position is `shard` modulo [`SHARDS`].
fn run_shard(shard: usize) {
    let Some(py) = python_or_skip("the mesh conformance suite") else {
        return;
    };
    let ids: Vec<String> = mandatory()
        .into_iter()
        .enumerate()
        .filter(|(i, _)| i % SHARDS == shard)
        .map(|(_, id)| id)
        .collect();
    let fixtures: Vec<String> = ids
        .iter()
        .map(|id| {
            let p = suite().join("fixtures").join(format!("{id}.json"));
            dunce::simplified(&p).display().to_string()
        })
        .collect();
    let recorder = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("support")
        .join("record_failures.py");
    let recorder = dunce::simplified(&recorder).display().to_string();
    assert!(
        !recorder.chars().any(char::is_whitespace),
        "the recorder's path has whitespace, which the suite's tools cannot pass: {recorder}"
    );
    let failures_dir = tempfile::tempdir().expect("a directory for failing commands");
    let native = native();
    let out = tool(&py, "run.py")
        .arg("--reference")
        .arg(&recorder)
        .arg("--participant")
        .arg(format!("codex={native}"))
        .arg("--participant")
        .arg(format!("localpilot={native}"))
        .env("LOCALPILOT_CONFORMANCE_FAILURES", failures_dir.path())
        .env(
            "LOCALPILOT_CONFORMANCE_REFERENCE",
            suite().join("reference").join("pair.py"),
        )
        .args(&fixtures)
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
        "conformance failed:\n{}\n{}\n{}",
        failures.join("\n"),
        report.lines().rev().take(5).collect::<Vec<_>>().join("\n"),
        failed_commands(failures_dir.path())
    );
    let summary = stdout
        .lines()
        .find(|l| l.starts_with("SELECTED "))
        .unwrap_or_default();
    // The expected count comes from the vendored mandatory list, not from the
    // runner's own report, so a runner that selects fewer fixtures fails here.
    let n = ids.len();
    assert!(n > 0, "shard {shard} is empty");
    assert_eq!(
        summary,
        format!("SELECTED {n} / TOTAL {n} / SKIPPED 0 / FAILED 0"),
        "every mandatory fixture in shard {shard} must run and pass"
    );
    assert!(
        !stdout.contains("MANDATORY_NOT_RUN"),
        "a mandatory fixture did not run:\n{stdout}"
    );
}
