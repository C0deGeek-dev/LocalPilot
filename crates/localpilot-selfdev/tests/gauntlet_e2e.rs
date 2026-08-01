//! End-to-end behaviour verification for the publish gauntlet.
//!
//! These build the *real* `localpilot` binary and drive the gauntlet against it,
//! so they exercise the two pieces a unit test cannot: reading a real
//! `version --json` and completing (or failing) a real RPC handshake. They are
//! `#[ignore]`d because each compiles the whole CLI; run them explicitly:
//!
//! ```text
//! cargo test -p localpilot-selfdev --test gauntlet_e2e -- --ignored
//! ```
// A test binary is allowed the unwrap/expect the workspace lint denies on
// library paths; `clippy.toml` relaxes it inside `#[cfg(test)]` unit modules, but
// an integration test is its own crate and needs the allow spelled out.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::Duration;

use localpilot_selfdev::{
    build, smoke_handshake, vet, write_smoke_config, BuildOptions, SelfDevError, SourceState,
};

/// The workspace root: this crate lives at `<root>/crates/localpilot-selfdev`.
fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root two levels up")
        .to_path_buf()
}

#[test]
#[ignore = "builds the whole CLI; run explicitly with --ignored"]
fn a_freshly_built_binary_passes_and_a_wrong_source_is_rejected() {
    let root = workspace_root();
    let source = SourceState::read(&root).expect("read workspace source");
    let target = std::env::temp_dir().join("selfdev-gauntlet-e2e");
    let built = build(&source, &BuildOptions::new(&target)).expect("build the candidate");

    let scratch = tempfile::tempdir().expect("scratch dir");

    // Healthy: the binary was built from exactly this source, so it passes.
    let reported = vet(
        &built.executable,
        &source,
        scratch.path(),
        Duration::from_secs(60),
    )
    .expect("a freshly built binary must pass its own gauntlet");
    assert_eq!(reported.git_hash, source.embedded_hash());
    assert_eq!(reported.fingerprint, source.fingerprint);

    // Stale: the same real binary, but checked against a source expectation whose
    // fingerprint does not match — the exact "built from other bytes" case. The
    // real `version --json` is read; only the expectation is wrong.
    let mut wrong = source.clone();
    wrong.fingerprint = "0000000000000000".to_string();
    let err = vet(
        &built.executable,
        &wrong,
        scratch.path(),
        Duration::from_secs(60),
    )
    .expect_err("a binary that does not match the source must be refused");
    assert!(matches!(err, SelfDevError::Invalid(m) if m.contains("fingerprint")));
}

#[test]
#[ignore = "builds the whole CLI; run explicitly with --ignored"]
fn a_candidate_that_cannot_handshake_in_time_is_rejected() {
    let root = workspace_root();
    let source = SourceState::read(&root).expect("read workspace source");
    let target = std::env::temp_dir().join("selfdev-gauntlet-e2e");
    let built = build(&source, &BuildOptions::new(&target)).expect("build the candidate");

    let scratch = tempfile::tempdir().expect("scratch dir");
    write_smoke_config(scratch.path(), "selfdev-smoke", "selfdev-smoke").expect("config");

    // A deadline no real session construction can meet: the handshake must fail
    // rather than hang, proving the timeout bounds a real process.
    let err = smoke_handshake(
        &built.executable,
        scratch.path(),
        "selfdev-smoke",
        "selfdev-smoke",
        Duration::from_millis(1),
    )
    .expect_err("an impossibly short deadline must reject, not hang");
    assert!(matches!(err, SelfDevError::Build { .. }));
}
