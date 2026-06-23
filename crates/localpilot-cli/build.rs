//! Embed a meaningful version string for the update check.
//!
//! Resolution order: an explicit `LOCALPILOT_VERSION` override (release builds),
//! then `git describe` (source builds), then the Cargo package version.

use std::process::Command;

fn main() {
    let version = std::env::var("LOCALPILOT_VERSION")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(git_describe)
        .unwrap_or_else(|| {
            std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string())
        });

    println!("cargo:rustc-env=LOCALPILOT_VERSION={version}");
    println!("cargo:rerun-if-changed=build.rs");
    emit_git_rerun_triggers();
}

/// Retrigger this build script when the checked-out commit moves, so the embedded
/// `git describe` version stays truthful after a pull + rebuild.
///
/// Watching only `.git/HEAD` is not enough: a commit on the *current* branch
/// advances the branch ref (`.git/refs/heads/<branch>` or `.git/packed-refs`),
/// not `HEAD` itself, which keeps the symbolic `ref: refs/heads/<branch>` line —
/// so same-branch commits left the version string stale. Watch HEAD (covers a
/// branch switch / detached checkout), the resolved branch ref (covers a loose
/// ref), and `packed-refs` (covers a packed tip). Best-effort: missing paths in a
/// source archive simply mean no retrigger.
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
    let output = Command::new("git")
        // Restrict to version tags so an unrelated tag (e.g. a branch marker)
        // is never picked up as the version.
        .args([
            "describe", "--tags", "--match", "v[0-9]*", "--always", "--dirty",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let described = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!described.is_empty()).then_some(described)
}
