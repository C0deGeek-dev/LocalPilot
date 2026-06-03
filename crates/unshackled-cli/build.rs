//! Embed a meaningful version string for the update check.
//!
//! Resolution order: an explicit `UNSHACKLED_VERSION` override (release builds),
//! then `git describe` (source builds), then the Cargo package version.

use std::process::Command;

fn main() {
    let version = std::env::var("UNSHACKLED_VERSION")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(git_describe)
        .unwrap_or_else(|| {
            std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string())
        });

    println!("cargo:rustc-env=UNSHACKLED_VERSION={version}");
    println!("cargo:rerun-if-changed=build.rs");
    // Rebuild when the checked-out commit moves (best-effort; absent in archives).
    println!("cargo:rerun-if-changed=../../.git/HEAD");
}

fn git_describe() -> Option<String> {
    let output = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let described = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!described.is_empty()).then_some(described)
}
