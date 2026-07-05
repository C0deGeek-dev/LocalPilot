//! End-to-end tests for folder ingestion commands.
#![allow(clippy::unwrap_used)]

use assert_cmd::Command;

fn run(dir: &std::path::Path, args: &[&str]) -> String {
    let output = localpilot_cmd()
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?} failed: {output:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn ingest_lifecycle_and_knowledge_commands_work() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".localpilot.toml"),
        "[ingest]\nenabled = true\nmax_files = 100\nmax_run_bytes = 100000\nmax_tokens = 100000\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("README.md"), "parser guide\n").unwrap();
    std::fs::write(dir.path().join("secret.md"), "token = abcdefghijklmnop\n").unwrap();

    let preview = run(dir.path(), &["ingest", "preview"]);
    assert!(preview.contains("candidate files:"));
    assert!(preview.contains("README.md"));

    let ingested = run(dir.path(), &["ingest", "run"]);
    assert!(ingested.contains("status: completed"));
    assert!(ingested.contains("chunks:"));

    let status = run(dir.path(), &["ingest", "status"]);
    assert!(status.contains("completed"));

    let search = run(dir.path(), &["knowledge", "search", "parser"]);
    assert!(search.contains("README.md"));

    let pack = run(dir.path(), &["knowledge", "pack", "parser task"]);
    assert!(pack.contains("task:"));
    assert!(pack.contains("budget:"));
    assert!(pack.contains("included ("));

    let review = run(dir.path(), &["ingest", "review"]);
    let id = review
        .lines()
        .next()
        .and_then(|line| line.split('\t').next())
        .unwrap()
        .to_string();
    let promoted = run(dir.path(), &["ingest", "promote", &id]);
    assert!(promoted.contains("queued"));

    let paused = run(dir.path(), &["ingest", "pause"]);
    assert!(paused.contains("paused"));
    // Resume continues the incomplete job from the persisted chunks (Refresh),
    // running it through to completion rather than only re-queueing it.
    let resumed = run(dir.path(), &["ingest", "resume"]);
    assert!(resumed.contains("resumed in refresh mode"));
    assert!(resumed.contains("status: completed"));
    let cancelled = run(dir.path(), &["ingest", "cancel"]);
    assert!(cancelled.contains("cancelled"));

    let forgotten = run(dir.path(), &["ingest", "forget", "README.md"]);
    assert!(forgotten.contains("removed"));

    let rebuilt = run(dir.path(), &["ingest", "rebuild"]);
    assert!(rebuilt.contains("deleted derived ingestion state"));
}

#[test]
fn ingest_run_works_with_default_config() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("README.md"), "parser guide\n").unwrap();

    let ingested = run(dir.path(), &["ingest", "run"]);

    assert!(ingested.contains("status: completed"));
    assert!(ingested.contains("chunks:"));
    let search = run(dir.path(), &["knowledge", "search", "parser"]);
    assert!(search.contains("README.md"));
}

#[test]
fn include_and_exclude_update_project_config() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("target")).unwrap();
    std::fs::write(dir.path().join("target").join("keep.md"), "keep").unwrap();

    let included = run(dir.path(), &["ingest", "include", "target/keep.md"]);
    assert!(included.contains("included target/keep.md"));
    let excluded = run(dir.path(), &["ingest", "exclude", "target/keep.md"]);
    assert!(excluded.contains("excluded target/keep.md"));

    let config = std::fs::read_to_string(dir.path().join(".localpilot.toml")).unwrap();
    assert!(config.contains("enabled = true"));
    assert!(config.contains("include"));
    assert!(config.contains("exclude"));
}

fn localpilot_cmd() -> Command {
    // The prebuilt test binary — never `cargo run` inside a test: nested
    // cargo fights the build-dir lock under nextest (a hang on Linux, an
    // exe-in-use failure on Windows) and re-resolves features.
    Command::new(env!("CARGO_BIN_EXE_localpilot"))
}
