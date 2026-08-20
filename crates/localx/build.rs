//! Embed a meaningful version string: an explicit environment override (release
//! and self-dev builds), then a `git describe` of the working tree, then the
//! crate's `Cargo.toml` version. A `git describe` of a dev build (e.g.
//! `v3.0.0-2-gabc1234`) parses to its base release tag, so `localx status` can
//! show a dev build as visibly distinct from a clean release.
//!
//! The stamp is kept parseable as a version even for a tree with no version tag
//! (a cargo git checkout, a shallow clone, a fork), where `git describe
//! --always` would otherwise fall back to a bare abbreviated sha. That is
//! defence in depth: the updater no longer decides whether a build is itself
//! from the stamp — it uses `std::env::current_exe` — but a bare-sha stamp is
//! what made the self-replace unreachable on the prerelease channel (LocalHub
//! #79), so this refuses to produce one.

use std::process::Command;

fn main() {
    let version = std::env::var("LOCALX_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(described_version)
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

/// A stamp that always parses as a version.
///
/// A tag-based `git describe` (`v3.3.1`, or `v3.3.1-2-gabc1234` past the tag) is
/// used as-is. A tagless tree yields a bare abbreviated sha from `--always`,
/// which is *not* a version; it is stamped `<crate version>-g<sha>` instead, so
/// it parses to the crate's base version while still naming the commit.
fn described_version() -> Option<String> {
    let described = git_describe()?;
    if is_tag_based(&described) {
        Some(described)
    } else {
        let base = std::env::var("CARGO_PKG_VERSION").ok()?;
        Some(format!("{base}-g{described}"))
    }
}

/// Whether a `git describe` output is tag-based (`v<digit>…`) rather than the
/// bare abbreviated sha `--always` falls back to when the tree has no version tag.
fn is_tag_based(described: &str) -> bool {
    described
        .strip_prefix('v')
        .and_then(|rest| rest.chars().next())
        .is_some_and(|c| c.is_ascii_digit())
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
