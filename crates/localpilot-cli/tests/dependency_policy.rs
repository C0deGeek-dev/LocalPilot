//! An audit exception for an unbuilt transport must not mask an active one.

use std::{path::Path, process::Command};

#[test]
fn optional_quic_stays_out_of_all_supported_workspace_feature_graphs() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let output = Command::new(env!("CARGO"))
        .env("CARGO_TERM_COLOR", "never")
        .args([
            "tree",
            "--locked",
            "--workspace",
            "--all-features",
            "--target",
            "all",
            "--edges",
            "normal,build,dev",
            "--prefix",
            "none",
            "--format",
            "{p}",
        ])
        .current_dir(root)
        .output()
        .expect("inspect the real Cargo graph");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let graph = String::from_utf8(output.stdout).expect("Cargo graph is UTF-8");
    let active: Vec<_> = graph
        .lines()
        .filter(|line| line.starts_with("quinn"))
        .collect();
    assert!(
        active.is_empty(),
        "QUIC is now active: remove its audit exceptions and adopt patched versions before shipping: {active:?}"
    );
}
