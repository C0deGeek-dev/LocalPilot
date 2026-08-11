//! Embed a meaningful version string: an explicit environment override (release
//! and self-dev builds), then a `git describe` of the working tree, then the
//! crate's `Cargo.toml` version. A `git describe` of a dev build (e.g.
//! `v3.0.0-2-gabc1234`) parses to its base release tag, so `localx status` can
//! show a dev build as visibly distinct from a clean release.

use std::process::Command;

fn main() {
    let version = std::env::var("LOCALX_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(git_describe)
        .unwrap_or_else(|| {
            std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string())
        });

    println!("cargo:rustc-env=LOCALX_VERSION={version}");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=LOCALX_VERSION");
    // Keep the version truthful after a checkout moves (best-effort; missing
    // paths in a source archive simply mean no retrigger).
    println!("cargo:rerun-if-changed=../../.git/HEAD");
}

fn git_describe() -> Option<String> {
    let output = Command::new("git")
        .args([
            "describe", "--tags", "--always", "--dirty", "--match", "v[0-9]*",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let described = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!described.is_empty()).then_some(described)
}
