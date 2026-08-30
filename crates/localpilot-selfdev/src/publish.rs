//! Build the working tree, vet it through the publish gauntlet, install it
//! immutably, and promote a channel to it — the guardrailed release step shared
//! by every caller that wants a vetted binary on a channel.
//!
//! This composes the crate's own primitives ([`build`], [`vet`], [`VersionStore`],
//! [`Channels`]) and nothing else, so a stale or broken build is refused *before*
//! any channel moves. It lives here, not in a caller, so the CLI release surface
//! and any higher-level sequencer share one definition rather than each
//! re-deriving the build→gauntlet→promote order.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::builder::{build, default_target_dir, executable_name, BuildOptions};
use crate::channel::{ChannelName, Channels};
use crate::error::SelfDevError;
use crate::gauntlet::{vet, DEFAULT_HANDSHAKE_TIMEOUT};
use crate::marker::BuildMarker;
use crate::source::SourceState;
use crate::store::{StoredVersion, VersionStore};

/// How many recent self-dev versions a promote keeps around; older ones are swept
/// so a copy-in store does not grow without bound. Channel-referenced versions are
/// always kept regardless of this count.
const KEEP_VERSIONS: usize = 5;

/// Build `workspace`, run it through the publish gauntlet, install it immutably,
/// promote `channel` to it, and sweep old versions. Progress lines are written to
/// `out`. Returns the freshly installed, vetted version.
///
/// The build output defaults to `<selfdev_root>/build-target` — deliberately
/// *outside* the workspace, so untracked build artefacts never feed into the
/// tree's own fingerprint on the next build; pass `target_dir` to override it.
///
/// # Errors
/// - [`SelfDevError::NotAWorkingTree`] if `workspace` is not a git tree;
/// - [`SelfDevError::Build`] if the candidate does not compile;
/// - [`SelfDevError::Io`] / [`SelfDevError::Invalid`] if the gauntlet rejects the
///   candidate or the install/promotion fails.
pub fn build_gauntlet_promote(
    workspace: &Path,
    selfdev_root: &Path,
    channel: ChannelName,
    target_dir: Option<PathBuf>,
    out: &mut dyn Write,
) -> Result<StoredVersion, SelfDevError> {
    let source = SourceState::read(workspace)?;
    let target = target_dir.unwrap_or_else(|| default_target_dir(selfdev_root));

    writeln!(out, "building {}...", source.version_label).map_err(SelfDevError::io)?;
    let built = build(&source, &BuildOptions::new(&target))?;

    writeln!(
        out,
        "vetting the candidate (identity, freshness, handshake)..."
    )
    .map_err(SelfDevError::io)?;
    let scratch = tempfile::tempdir().map_err(SelfDevError::io)?;
    let reported = vet(
        &built.executable,
        &source,
        scratch.path(),
        DEFAULT_HANDSHAKE_TIMEOUT,
    )?;

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
    let installed = store.install(&source.version_label, &built.executable, &marker)?;
    channels.set(channel, &installed.label)?;

    // Reclaim disk: keep the recent versions plus everything a channel points at.
    let reclaimed = store.sweep(KEEP_VERSIONS, &channels.active_targets());
    if !reclaimed.is_empty() {
        writeln!(out, "reclaimed {} old version(s)", reclaimed.len()).map_err(SelfDevError::io)?;
    }
    Ok(installed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::CURRENT;

    #[test]
    fn building_outside_a_git_tree_is_a_typed_error() {
        let temp = tempfile::tempdir().expect("temp");
        // Fence git's walk-up so a repo above the temp root cannot be found.
        let previous = std::env::var_os("GIT_CEILING_DIRECTORIES");
        std::env::set_var("GIT_CEILING_DIRECTORIES", temp.path());
        let mut out = Vec::new();
        let result =
            build_gauntlet_promote(temp.path(), temp.path(), CURRENT.into(), None, &mut out);
        match previous {
            Some(v) => std::env::set_var("GIT_CEILING_DIRECTORIES", v),
            None => std::env::remove_var("GIT_CEILING_DIRECTORIES"),
        }
        assert!(
            matches!(result, Err(SelfDevError::NotAWorkingTree(_))),
            "a non-git directory must be a typed working-tree error"
        );
    }
}
