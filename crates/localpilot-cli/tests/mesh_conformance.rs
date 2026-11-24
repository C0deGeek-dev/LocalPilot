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
use std::process::Command;

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

/// The `start` flags that turn on an optional session feature, as the
/// vendored runner lists them.
const OPTIONAL_FEATURE_FLAGS: &[&str] = &["--review-snapshot"];

/// Whether fixture `id` starts a session with an optional feature.
fn uses_optional_feature(id: &str) -> bool {
    let fixture: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(suite().join("fixtures").join(format!("{id}.json")))
            .expect("the fixture"),
    )
    .expect("the fixture is JSON");
    fixture["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|step| {
            let cmds: Vec<&serde_json::Value> = step.get("cmd").map_or_else(
                || step["parallel"].as_array().into_iter().flatten().collect(),
                |c| vec![c],
            );
            cmds.iter().any(|c| {
                c.as_array().into_iter().flatten().any(|a| {
                    a.as_str()
                        .is_some_and(|a| OPTIONAL_FEATURE_FLAGS.contains(&a))
                })
            })
        })
}

#[test]
fn the_vendored_fixtures_are_exactly_the_mandatory_list() {
    let mut on_disk: Vec<String> = std::fs::read_dir(suite().join("fixtures"))
        .expect("the vendored fixtures")
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".json").map(str::to_owned)
        })
        .collect();
    // A fixture that turns on an optional session feature at `start` is
    // not on the mandatory list (the runner's selection rule); it is run
    // through its own test once this participant claims the feature.
    on_disk.retain(|id| !uses_optional_feature(id));
    on_disk.sort();
    let mut listed = mandatory();
    listed.sort();
    assert_eq!(
        on_disk, listed,
        "a vendored fixture is not mandatory, or a mandatory one is missing"
    );
}

/// The optional-feature fixtures, which the vendored runner leaves out of
/// the participant profile, run against `localpilot mesh` now that it
/// claims the feature: through a copy of the suite whose runner selects
/// them. The vendored files themselves stay byte-for-byte as vendored.
#[test]
fn the_optional_feature_fixtures_pass_now_that_the_feature_is_claimed() {
    let Some(py) = python_or_skip("the optional-feature fixtures") else {
        return;
    };
    assert!(localpilot_mesh::FEATURES.contains(&"review-snapshot"));
    let copy = tempfile::tempdir().expect("a copy of the suite");
    copy_tree(&suite(), copy.path());
    let runner = copy.path().join("run.py");
    let text = std::fs::read_to_string(&runner).expect("run.py");
    let skip = r#"OPTIONAL_FEATURE_FLAGS = {"--review-snapshot"}"#;
    assert_eq!(
        text.matches(skip).count(),
        1,
        "the runner's optional-feature skip"
    );
    std::fs::write(
        &runner,
        text.replace(skip, "OPTIONAL_FEATURE_FLAGS = set()"),
    )
    .expect("write the copy's runner");
    let mut ids: Vec<String> = std::fs::read_dir(suite().join("fixtures"))
        .expect("the vendored fixtures")
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".json").map(str::to_owned)
        })
        .filter(|id| uses_optional_feature(id))
        .collect();
    ids.sort();
    assert!(!ids.is_empty(), "no optional-feature fixtures are vendored");
    let native = native();
    let out = Command::new(&py[0])
        .args(&py[1..])
        .arg(&runner)
        .current_dir(copy.path())
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("LOCALPILOT_MESH__WRITER", "native")
        .arg("--participant")
        .arg(format!("codex={native}"))
        .arg("--participant")
        .arg(format!("localpilot={native}"))
        .args(ids.iter().map(|id| format!("fixtures/{id}.json")))
        .output()
        .expect("run the copied runner");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // A fixture no native role takes part in is skipped; every other one
    // must have been selected and passed.
    let skipped: Vec<&str> = stdout.lines().filter(|l| l.starts_with("SKIP ")).collect();
    assert!(skipped.iter().all(|l| l.contains("takes part")), "{stdout}");
    let n = ids.len();
    let s = skipped.len();
    assert!(
        n - s >= 3,
        "too few optional-feature fixtures ran:\n{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "SELECTED {} / TOTAL {n} / SKIPPED {s} / FAILED 0",
            n - s
        )),
        "{stdout}"
    );
}

/// Copy a directory tree, files only.
fn copy_tree(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).expect("a directory to copy") {
        let entry = entry.expect("a directory entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("a file type").is_dir() {
            std::fs::create_dir_all(&target).expect("a directory");
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("a copied file");
        }
    }
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
