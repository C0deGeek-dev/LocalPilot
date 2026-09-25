//! `localpilot mesh` and the reference implementation write one live mailbox
//! at the same time without losing, duplicating or tearing anything: the
//! vendored suite's mixed-writer soak, with the reference as implementation
//! A and this binary as B.
//!
//! A short run is part of every test run. The long run is ignored by default
//! and runs nightly (`cargo test -p localpilot --test mesh_soak -- --ignored`),
//! with `LOCALPILOT_SOAK_POSTS` posts per poster per session (default 200).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use support::{native, python_or_skip, tool};

fn soak(posts: u32) {
    let Some(py) = python_or_skip("the mesh soak") else {
        return;
    };
    let out = tool(&py, "soak.py")
        .arg("--impl-b")
        .arg(native())
        .arg("--posts")
        .arg(posts.to_string())
        .output()
        .expect("run the soak");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stdout.contains("SOAK ok"),
        "the soak failed:\n{stdout}\n{stderr}"
    );
    eprintln!("{}", stdout.trim());
}

#[test]
fn a_short_mixed_writer_soak_passes() {
    soak(20);
}

#[test]
#[ignore = "long: runs nightly"]
fn the_long_mixed_writer_soak_passes() {
    let posts = std::env::var("LOCALPILOT_SOAK_POSTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    soak(posts);
}
