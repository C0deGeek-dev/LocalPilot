//! `localpilot selfdev` — build LocalPilot from its own source, vet the result,
//! and promote it, from the command line.
//!
//! This is the **manual** self-dev surface: a developer or a CI job drives the
//! build → gauntlet → publish flow explicitly. It is deliberately *not* the
//! autonomous in-session loop where the model builds and reloads itself — that
//! stays an opt-in product decision, off by default (ADR-0128). Every command
//! here composes the `localpilot-selfdev` primitives and nothing more.
//!
//! The functions take the workspace root and the self-dev data root explicitly so
//! they are testable against a scratch directory; the CLI wires the current
//! directory and the standard per-user data root.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use localpilot_selfdev::{
    build, executable_name, vet, AutoReloadBreaker, BuildMarker, BuildOptions, ChannelName,
    Channels, SourceState, VersionStore, CURRENT, DEFAULT_HANDSHAKE_TIMEOUT, SLOW, STABLE,
};

/// How many failed auto-reloads the breaker tolerates. Reported by `status`; the
/// autonomous loop that would consume it is deferred (ADR-0128).
const AUTO_RELOAD_LIMIT: u32 = 3;

/// `selfdev build`: fingerprint the working tree and build it, reporting the
/// source label and where the binary landed.
///
/// The build output defaults to `<selfdev_root>/build-target` — deliberately
/// *outside* the workspace, so the untracked build artefacts never feed into the
/// tree's own fingerprint on the next build.
///
/// # Errors
/// Returns an error if `workspace` is not a git working tree or the build fails.
pub fn run_build(
    workspace: &Path,
    selfdev_root: &Path,
    target_dir: Option<PathBuf>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let source = SourceState::read(workspace).context("reading the source tree")?;
    let target = target_dir.unwrap_or_else(|| localpilot_selfdev::default_target_dir(selfdev_root));
    writeln!(
        out,
        "building {} (git {}, {})",
        source.version_label,
        source.embedded_hash(),
        if source.dirty { "dirty" } else { "clean" }
    )?;
    let built = build(&source, &BuildOptions::new(&target)).context("building the candidate")?;
    writeln!(out, "built: {}", built.executable.display())?;
    Ok(())
}

/// `selfdev publish`: build, run the publish gauntlet, and — only if it passes —
/// install the binary immutably and point a channel at it.
///
/// This is the guardrailed release step: a stale or broken build is refused
/// before any channel moves.
///
/// # Errors
/// Returns an error if the build fails, the gauntlet rejects the candidate, or
/// the install/promotion fails.
pub fn run_publish(
    workspace: &Path,
    selfdev_root: &Path,
    channel: &str,
    target_dir: Option<PathBuf>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let channel = ChannelName::parse(channel).context("channel name")?;
    let source = SourceState::read(workspace).context("reading the source tree")?;
    let target = target_dir.unwrap_or_else(|| localpilot_selfdev::default_target_dir(selfdev_root));

    writeln!(out, "building {}...", source.version_label)?;
    let built = build(&source, &BuildOptions::new(&target)).context("building the candidate")?;

    writeln!(
        out,
        "vetting the candidate (identity, freshness, handshake)..."
    )?;
    let scratch = tempfile::tempdir().context("scratch dir for the gauntlet")?;
    let reported = vet(
        &built.executable,
        &source,
        scratch.path(),
        DEFAULT_HANDSHAKE_TIMEOUT,
    )
    .context("the candidate did not pass the publish gauntlet")?;

    let store = VersionStore::new(selfdev_root);
    let channels = Channels::new(selfdev_root);
    let marker = BuildMarker::new(
        source.version_label.clone(),
        source.embedded_hash(),
        source.fingerprint.clone(),
        source.dirty,
        reported.version,
        executable_name(),
    );
    let installed = store
        .install(&source.version_label, &built.executable, &marker)
        .context("installing the vetted binary")?;
    channels
        .set(channel.clone(), &installed.label)
        .context("promoting the channel")?;

    writeln!(
        out,
        "published {} to channel '{}'",
        installed.label,
        channel.as_str()
    )?;
    Ok(())
}

/// `selfdev status`: what is installed, what each channel points at, and the
/// auto-reload breaker's state.
///
/// # Errors
/// Never fails on an empty or missing store — it reports "nothing installed".
pub fn run_status(selfdev_root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let store = VersionStore::new(selfdev_root);
    let channels = Channels::new(selfdev_root);
    let breaker = AutoReloadBreaker::new(selfdev_root, AUTO_RELOAD_LIMIT);

    let installed = store.installed();
    if installed.is_empty() {
        writeln!(out, "no self-dev versions installed")?;
    } else {
        writeln!(out, "installed self-dev versions:")?;
        for version in &installed {
            writeln!(
                out,
                "  {} (git {}{})",
                version.label,
                version.marker.git_hash,
                if version.marker.dirty { ", dirty" } else { "" }
            )?;
        }
    }

    writeln!(out, "channels:")?;
    for channel in [CURRENT, STABLE, SLOW] {
        match channels.label(channel) {
            Some(label) => writeln!(out, "  {} -> {label}", channel.0)?,
            None => writeln!(out, "  {} -> (unset)", channel.0)?,
        }
    }

    writeln!(
        out,
        "auto-reload attempts: {}/{}{}",
        breaker.count(),
        AUTO_RELOAD_LIMIT,
        if breaker.is_tripped() {
            " (tripped)"
        } else {
            ""
        }
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_of_an_empty_root_reports_nothing_installed_and_unset_channels() {
        let temp = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        run_status(temp.path(), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();

        assert!(text.contains("no self-dev versions installed"));
        assert!(text.contains("current -> (unset)"));
        assert!(text.contains("auto-reload attempts: 0/3"));
    }

    #[test]
    fn status_reports_an_installed_version_and_its_channel() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let store = VersionStore::new(root);
        let channels = Channels::new(root);

        // Install a stand-in version and point `current` at it.
        let src = root.join("built");
        std::fs::write(&src, "bin").unwrap();
        let marker = BuildMarker::new(
            "ab12cd3",
            "ab12cd3def",
            "fp",
            false,
            "2.6.0-selfdev-ab12cd3",
            executable_name(),
        );
        store.install("ab12cd3", &src, &marker).unwrap();
        channels.set(CURRENT, "ab12cd3").unwrap();

        let mut out = Vec::new();
        run_status(root, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();

        assert!(text.contains("ab12cd3 (git ab12cd3def)"));
        assert!(text.contains("current -> ab12cd3"));
    }

    #[test]
    #[ignore = "builds the whole CLI; run explicitly with --ignored"]
    fn publish_builds_vets_and_promotes_and_status_then_shows_it() {
        // End-to-end release automation against the real workspace, offline: build
        // the CLI, run the gauntlet, install, promote — then confirm `status` sees
        // it. The build target and self-dev root are scratch dirs, so nothing
        // touches the developer's real data or the repo tree.
        let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(std::path::Path::parent)
            .expect("workspace root")
            .to_path_buf();
        let scratch = tempfile::tempdir().unwrap();
        let target = scratch.path().join("build-target");

        let mut out = Vec::new();
        run_publish(
            &workspace,
            scratch.path(),
            "current",
            Some(target),
            &mut out,
        )
        .expect("publish should build, vet, and promote");
        let published = String::from_utf8(out).unwrap();
        assert!(
            published.contains("published") && published.contains("channel 'current'"),
            "publish must report the promotion: {published}"
        );

        let mut status = Vec::new();
        run_status(scratch.path(), &mut status).unwrap();
        let status = String::from_utf8(status).unwrap();
        assert!(
            status.contains("current -> ") && !status.contains("current -> (unset)"),
            "status must show the promoted channel: {status}"
        );
        assert!(status.contains("installed self-dev versions:"));
    }

    #[test]
    fn building_outside_a_git_tree_is_a_clear_error() {
        let temp = tempfile::tempdir().unwrap();
        // Fence git's walk-up so a repo above the temp root can't be found.
        let previous = std::env::var_os("GIT_CEILING_DIRECTORIES");
        std::env::set_var("GIT_CEILING_DIRECTORIES", temp.path());
        let mut out = Vec::new();
        let result = run_build(temp.path(), temp.path(), None, &mut out);
        match previous {
            Some(v) => std::env::set_var("GIT_CEILING_DIRECTORIES", v),
            None => std::env::remove_var("GIT_CEILING_DIRECTORIES"),
        }
        assert!(result.is_err(), "a non-git directory must not build");
    }
}
