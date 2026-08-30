//! Which stored version a channel points at — a marker file, on every platform.
//!
//! A channel (`current`, `stable`, a slow lane) is one small JSON file naming a
//! version label. Pointing a channel somewhere new is: write a temporary file,
//! then rename it over the old one. `rename` replaces a file atomically on both
//! POSIX and Windows, so a reader ever sees the old label or the new one, never a
//! half-written pointer, and never nothing.
//!
//! **Why not a symlink.** The obvious pointer is a directory symlink. But Windows
//! directory symlinks need a privilege or Developer Mode, and junctions need raw
//! `DeviceIoControl` — impossible under this workspace's `unsafe_code = "forbid"`
//! without a new dependency. A marker file needs neither, is identical on every
//! platform, and makes one of the reference's hard-won lessons — *compare the
//! payload that runs, not the wrapper* — structural rather than a discipline:
//! there is no wrapper to be fooled by, because resolution always ends at an
//! immutable version directory.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::SelfDevError;
use crate::store::{StoredVersion, VersionStore};

/// The channel that a freshly built binary is promoted onto.
pub const CURRENT: Channel = Channel("current");
/// The channel a known-good binary is held on.
pub const STABLE: Channel = Channel("stable");
/// A slower lane, for a version promoted only after longer soak.
pub const SLOW: Channel = Channel("slow");

/// Directory holding the channel marker files.
const CHANNELS_DIR: &str = "channels";
/// Suffix for a channel's marker file.
const MARKER_SUFFIX: &str = ".json";

/// A named channel. The name is a bare identifier so it is always a safe file
/// name; [`Channel::parse`] rejects anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Channel(pub &'static str);

/// A channel name supplied at runtime, validated to a safe file name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelName(String);

impl ChannelName {
    /// Validate `name` as a channel: ASCII alphanumeric, `-`, or `_`, non-empty.
    ///
    /// # Errors
    /// Returns [`SelfDevError::Invalid`] for anything that could escape the
    /// channels directory or is otherwise not a bare name.
    pub fn parse(name: &str) -> Result<Self, SelfDevError> {
        let ok = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if ok {
            Ok(Self(name.to_string()))
        } else {
            Err(SelfDevError::Invalid(format!(
                "not a valid channel name: {name:?}"
            )))
        }
    }

    /// The name as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<Channel> for ChannelName {
    fn from(channel: Channel) -> Self {
        Self(channel.0.to_string())
    }
}

/// The on-disk record a channel marker holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ChannelMarker {
    /// The version label this channel resolves to.
    label: String,
}

/// The channel pointers rooted at a self-dev subtree.
#[derive(Debug, Clone)]
pub struct Channels {
    root: PathBuf,
}

impl Channels {
    /// Channel pointers rooted at `root` (the self-dev subtree).
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The marker file backing `channel`.
    fn marker_path(&self, channel: &ChannelName) -> PathBuf {
        self.root
            .join(CHANNELS_DIR)
            .join(format!("{}{MARKER_SUFFIX}", channel.as_str()))
    }

    /// Point `channel` at `label`, atomically.
    ///
    /// The label is written to a temporary file that is then renamed over the
    /// channel's marker. A reader concurrent with the swap sees the whole old
    /// marker or the whole new one. The label is *not* required to be installed:
    /// the caller (the publish gauntlet) decides what may be promoted; this layer
    /// only records the pointer.
    ///
    /// # Errors
    /// Returns [`SelfDevError::Io`] when the directory cannot be created or the
    /// write/rename fails.
    pub fn set(&self, channel: impl Into<ChannelName>, label: &str) -> Result<(), SelfDevError> {
        let channel = channel.into();
        let dir = self.root.join(CHANNELS_DIR);
        std::fs::create_dir_all(&dir).map_err(SelfDevError::io)?;
        let marker = ChannelMarker {
            label: label.to_string(),
        };
        let body = serde_json::to_vec_pretty(&marker).map_err(SelfDevError::io)?;
        let temp = dir.join(format!(
            ".{}.{}.incoming",
            channel.as_str(),
            std::process::id()
        ));
        std::fs::write(&temp, body).map_err(SelfDevError::io)?;
        std::fs::rename(&temp, self.marker_path(&channel)).map_err(|error| {
            let _ = std::fs::remove_file(&temp);
            SelfDevError::io(error)
        })
    }

    /// The label `channel` points at, if it is set.
    #[must_use]
    pub fn label(&self, channel: impl Into<ChannelName>) -> Option<String> {
        let channel = channel.into();
        let body = std::fs::read_to_string(self.marker_path(&channel)).ok()?;
        let marker: ChannelMarker = serde_json::from_str(&body).ok()?;
        Some(marker.label)
    }

    /// The labels the built-in channels (`current`, `stable`, `slow`) currently
    /// resolve to — the set a version sweep must never reclaim, since a channel
    /// points at each (or a process may be running from it).
    #[must_use]
    pub fn active_targets(&self) -> Vec<String> {
        [CURRENT, STABLE, SLOW]
            .into_iter()
            .filter_map(|channel| self.label(channel))
            .collect()
    }

    /// Resolve `channel` all the way to the immutable stored version it names.
    ///
    /// Returns `None` when the channel is unset, or points at a label that is not
    /// installed (or whose install is torn). The returned [`StoredVersion`] is an
    /// immutable directory — never the channel marker — so a caller reads the
    /// payload that actually runs.
    #[must_use]
    pub fn resolve(
        &self,
        store: &VersionStore,
        channel: impl Into<ChannelName>,
    ) -> Option<StoredVersion> {
        store.get(&self.label(channel)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::marker::BuildMarker;

    fn install(store: &VersionStore, label: &str, body: &str) -> StoredVersion {
        let src_dir = store.root().join(".src");
        std::fs::create_dir_all(&src_dir).expect("src dir");
        let src = src_dir.join(format!("built-{label}"));
        std::fs::write(&src, body).expect("write src");
        let marker = BuildMarker::new(
            label,
            "abc1234",
            "fp",
            true,
            format!("2.6.0-selfdev-{label}"),
            localpilot_dist::executable_name(crate::builder::TOOL),
        );
        store.install(label, &src, &marker).expect("install")
    }

    #[test]
    fn a_channel_resolves_to_the_immutable_version_it_names() {
        let temp = tempfile::tempdir().expect("temp");
        let store = VersionStore::new(temp.path());
        let channels = Channels::new(temp.path());
        install(&store, "aaaa", "one");

        channels.set(CURRENT, "aaaa").expect("set");

        let resolved = channels.resolve(&store, CURRENT).expect("resolve");
        assert_eq!(resolved.label, "aaaa");
        assert_eq!(
            std::fs::read_to_string(resolved.executable()).unwrap(),
            "one"
        );
        // Resolution ends at a version directory, never at the channel marker.
        assert!(resolved.dir.starts_with(store.version_dir("aaaa")));
    }

    #[test]
    fn a_channel_swap_is_atomic_and_repoints_cleanly() {
        let temp = tempfile::tempdir().expect("temp");
        let store = VersionStore::new(temp.path());
        let channels = Channels::new(temp.path());
        install(&store, "aaaa", "one");
        install(&store, "bbbb", "two");

        channels.set(CURRENT, "aaaa").expect("set a");
        assert_eq!(channels.label(CURRENT).as_deref(), Some("aaaa"));

        channels.set(CURRENT, "bbbb").expect("set b");
        assert_eq!(channels.label(CURRENT).as_deref(), Some("bbbb"));
        assert_eq!(
            std::fs::read_to_string(channels.resolve(&store, CURRENT).unwrap().executable())
                .unwrap(),
            "two"
        );
    }

    #[test]
    fn a_swap_leaves_the_previous_version_intact_for_a_process_running_from_it() {
        let temp = tempfile::tempdir().expect("temp");
        let store = VersionStore::new(temp.path());
        let channels = Channels::new(temp.path());

        let old = install(&store, "aaaa", "one");
        channels.set(CURRENT, "aaaa").expect("point at old");
        // A process is now "running" from this exact path.
        let running_from = old.executable();

        install(&store, "bbbb", "two");
        channels.set(CURRENT, "bbbb").expect("swap to new");

        assert!(
            running_from.is_file(),
            "the old binary's path must survive a channel swap"
        );
        assert_eq!(
            std::fs::read_to_string(&running_from).unwrap(),
            "one",
            "a swap must not mutate the path a live process was exec'd from"
        );
    }

    #[test]
    fn separate_channels_point_independently() {
        let temp = tempfile::tempdir().expect("temp");
        let store = VersionStore::new(temp.path());
        let channels = Channels::new(temp.path());
        install(&store, "aaaa", "one");
        install(&store, "bbbb", "two");

        channels.set(CURRENT, "bbbb").expect("current");
        channels.set(STABLE, "aaaa").expect("stable");
        channels.set(SLOW, "aaaa").expect("slow");

        assert_eq!(channels.label(CURRENT).as_deref(), Some("bbbb"));
        assert_eq!(channels.label(STABLE).as_deref(), Some("aaaa"));
        assert_eq!(channels.label(SLOW).as_deref(), Some("aaaa"));
    }

    #[test]
    fn an_unset_channel_and_a_dangling_one_both_resolve_to_nothing() {
        let temp = tempfile::tempdir().expect("temp");
        let store = VersionStore::new(temp.path());
        let channels = Channels::new(temp.path());

        assert!(channels.resolve(&store, CURRENT).is_none());
        // Pointing at a label that was never installed resolves to nothing, not
        // to a phantom version.
        channels.set(CURRENT, "ghost").expect("set");
        assert_eq!(channels.label(CURRENT).as_deref(), Some("ghost"));
        assert!(channels.resolve(&store, CURRENT).is_none());
    }

    #[test]
    fn a_traversing_channel_name_is_refused() {
        assert!(ChannelName::parse("../escape").is_err());
        assert!(ChannelName::parse("a/b").is_err());
        assert!(ChannelName::parse("").is_err());
        assert!(ChannelName::parse("current").is_ok());
        assert!(ChannelName::parse("slow-2").is_ok());
    }
}
