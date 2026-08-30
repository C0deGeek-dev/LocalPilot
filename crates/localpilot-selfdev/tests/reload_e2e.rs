//! End-to-end behaviour verification for the reload staging + continuation
//! lifecycle, against the *real* CLI binary — offline, no model needed.
//!
//! It proves the parts a unit test cannot: that a staged reload installs the
//! genuine built binary immutably, that the successor path really comes up on
//! that new binary (via its `version --json` identity), and that the continuation
//! intent is durable and delivered exactly once. The one thing it does not do is
//! the actual in-place `exec`, which would replace the test process itself — that
//! final syscall is proven by `relaunch`'s own tests and exercised live when the
//! opt-in self-dev surface lands.
//!
//! `#[ignore]` because it compiles the whole CLI; run explicitly:
//!
//! ```text
//! cargo test -p localpilot-selfdev --test reload_e2e -- --ignored
//! ```
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::process::{Command, Stdio};

use localpilot_selfdev::{
    build, stage_reload, BuildMarker, BuildOptions, Channels, ReloadIntent, ReloadRequest,
    ReloadStore, SourceState, VersionStore, CURRENT,
};

fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root two levels up")
        .to_path_buf()
}

#[test]
#[ignore = "builds the whole CLI; run explicitly with --ignored"]
fn staging_a_reload_installs_the_real_binary_and_the_successor_runs_it() {
    let root = workspace_root();
    let source = SourceState::read(&root).expect("read source");
    let target = std::env::temp_dir().join("selfdev-reload-e2e");
    let built = build(&source, &BuildOptions::new(&target)).expect("build the candidate");

    // A self-dev subtree entirely inside the test's scratch space.
    let scratch = tempfile::tempdir().expect("scratch");
    let store = VersionStore::new(scratch.path());
    let channels = Channels::new(scratch.path());
    let reload = ReloadStore::new(scratch.path());

    let marker = BuildMarker::new(
        source.version_label.clone(),
        source.embedded_hash(),
        source.fingerprint.clone(),
        source.dirty,
        format!("2.6.0-selfdev-{}", source.version_label),
        localpilot_selfdev::executable_name(),
    );
    let intent = ReloadIntent::new(
        "req-e2e",
        "sess-e2e",
        "2.6.0-old",
        &marker.version,
        "finish the reload work",
        None,
        std::process::id(),
    );
    let request = ReloadRequest {
        store: &store,
        channels: &channels,
        reload: &reload,
        executable: &built.executable,
        marker: &marker,
        channel: CURRENT.into(),
        intent,
        successor_args: vec!["version".to_string(), "--json".to_string()],
    };

    // Stage: install immutably, promote the channel, write the intent.
    let program = stage_reload(&request).expect("stage the reload");
    assert!(
        program.starts_with(store.version_dir(&source.version_label)),
        "the successor path must be inside the immutable version store"
    );

    // The successor really comes up on the new binary: run the staged program and
    // read the identity it reports. This is the "process comes up on the new
    // binary" guarantee, minus the exec that would replace this test process.
    let output = Command::new(&program)
        .args(["version", "--json"])
        .stdin(Stdio::null())
        .output()
        .expect("run the staged binary");
    assert!(output.status.success(), "the staged binary must run");
    let reported: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("version --json");
    assert_eq!(
        reported["git_hash"].as_str(),
        Some(source.embedded_hash()),
        "the staged binary must be the one built from this source"
    );
    assert_eq!(
        reported["fingerprint"].as_str(),
        Some(source.fingerprint.as_str())
    );

    // The continuation intent is durable and pending until delivery is recorded.
    let pending = reload.pending("sess-e2e").expect("a pending continuation");
    assert!(pending
        .continuation_prompt()
        .contains("finish the reload work"));
    reload.mark_delivered("sess-e2e").expect("mark delivered");
    assert!(
        reload.pending("sess-e2e").is_none(),
        "a delivered continuation is never offered again"
    );
}
