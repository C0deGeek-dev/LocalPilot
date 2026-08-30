//! Embed a meaningful version string and the commit it was built from.
//!
//! Resolution order for each value: an explicit environment override (release
//! builds, and the self-dev build wrapper), then the repository, then a
//! fallback. The policy — including whether the repository has to be watched at
//! all — lives in `build_meta.rs`, which the crate's tests include and assert.

use std::process::Command;

#[path = "build_meta.rs"]
mod build_meta;

fn main() {
    let meta = build_meta::resolve(
        std::env::var("LOCALPILOT_VERSION").ok(),
        std::env::var("LOCALPILOT_GIT_HASH").ok(),
        std::env::var("LOCALPILOT_SOURCE_FINGERPRINT").ok(),
        &std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string()),
        git_describe,
        git_head,
    );

    println!("cargo:rustc-env=LOCALPILOT_VERSION={}", meta.version);
    println!("cargo:rustc-env=LOCALPILOT_GIT_HASH={}", meta.git_hash);
    println!(
        "cargo:rustc-env=LOCALPILOT_SOURCE_FINGERPRINT={}",
        meta.fingerprint.unwrap_or_default()
    );

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build_meta.rs");
    // The values that can come from the environment are watched by name, so a
    // caller that supplies them gets a rebuild exactly when they change.
    println!("cargo:rerun-if-env-changed=LOCALPILOT_VERSION");
    println!("cargo:rerun-if-env-changed=LOCALPILOT_GIT_HASH");
    println!("cargo:rerun-if-env-changed=LOCALPILOT_SOURCE_FINGERPRINT");
    if meta.watch_git {
        emit_git_rerun_triggers();
    }
}

/// Retrigger this build script when the checked-out commit moves, so a version
/// or hash *read from the repository* stays truthful after a pull + rebuild.
///
/// Watching only `.git/HEAD` is not enough: a commit on the *current* branch
/// advances the branch ref (`.git/refs/heads/<branch>` or `.git/packed-refs`),
/// not `HEAD` itself, which keeps the symbolic `ref: refs/heads/<branch>` line —
/// so same-branch commits left the version string stale. Watch HEAD (covers a
/// branch switch / detached checkout), the resolved branch ref (covers a loose
/// ref), and `packed-refs` (covers a packed tip). Best-effort: missing paths in a
/// source archive simply mean no retrigger.
///
/// Skipped entirely when the caller supplied both values, because then nothing
/// this script emits depends on the repository — and the self-dev loop rebuilds
/// often enough that an unnecessary full rebuild per commit is the difference
/// between a usable inner loop and an unusable one.
fn emit_git_rerun_triggers() {
    // build.rs runs with the crate manifest dir as the working directory; the
    // repo's `.git` is two levels up (`<repo>/crates/localpilot-cli`).
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/packed-refs");
    if let Ok(head) = std::fs::read_to_string("../../.git/HEAD") {
        if let Some(reference) = head.strip_prefix("ref:").map(str::trim) {
            if !reference.is_empty() {
                println!("cargo:rerun-if-changed=../../.git/{reference}");
            }
        }
    }
}

fn git_describe() -> Option<String> {
    // Restrict to version tags so an unrelated tag (e.g. a branch marker) is
    // never picked up as the version.
    git(&[
        "describe", "--tags", "--match", "v[0-9]*", "--always", "--dirty",
    ])
}

fn git_head() -> Option<String> {
    git(&["rev-parse", "HEAD"])
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!text.is_empty()).then_some(text)
}
