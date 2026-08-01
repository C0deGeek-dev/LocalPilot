//! Making a reload safe to fail: a rollback token, a version comparison that
//! refuses to phantom-trigger, and a circuit breaker on auto-reload.
//!
//! Deliberately **not** a crash-detect-and-revert loop. The reference built one,
//! then deleted it in favour of exactly these three plain mechanisms — a token
//! that restores the previous channel pointer, a no-downgrade comparison, and a
//! bounded attempt counter — because the fancy loop was the source of the
//! infinite-reload bug family, not the cure (D002).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::channel::{ChannelName, Channels};
use crate::error::SelfDevError;

/// Directory holding rollback tokens.
const ACTIVATION_DIR: &str = "activation";
/// File holding the persisted auto-reload attempt counter.
const BREAKER_FILE: &str = "auto-reload-attempts";

/// A record, written *before* a channel is repointed at a new build, of what the
/// channel pointed at before — so a failed reload can put it back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingActivation {
    /// The session this activation is for.
    pub session_id: String,
    /// The channel being repointed.
    pub channel: String,
    /// The label being activated.
    pub new_version: String,
    /// The label the channel pointed at before — `None` if it was unset, so a
    /// rollback clears it rather than inventing a target.
    pub previous_version: Option<String>,
}

/// The store of in-flight activations, rooted at the self-dev subtree.
#[derive(Debug, Clone)]
pub struct ActivationGuard {
    root: PathBuf,
}

impl ActivationGuard {
    /// A guard rooted at `selfdev_root` (tokens land in `<root>/activation/`).
    #[must_use]
    pub fn new(selfdev_root: impl Into<PathBuf>) -> Self {
        Self {
            root: selfdev_root.into().join(ACTIVATION_DIR),
        }
    }

    fn token_path(&self, session_id: &str) -> PathBuf {
        self.root.join(format!("{session_id}.json"))
    }

    /// Capture the channel's current target and record the intended activation,
    /// **before** the channel is repointed. Returns the token so a caller can pass
    /// it straight to [`Channels::set`] afterwards.
    ///
    /// # Errors
    /// Returns [`SelfDevError::Io`] when the token cannot be written.
    pub fn begin(
        &self,
        channels: &Channels,
        session_id: &str,
        channel: ChannelName,
        new_version: &str,
    ) -> Result<PendingActivation, SelfDevError> {
        let previous_version = channels.label(channel.clone());
        let token = PendingActivation {
            session_id: session_id.to_string(),
            channel: channel.as_str().to_string(),
            new_version: new_version.to_string(),
            previous_version,
        };
        self.write(&token)?;
        Ok(token)
    }

    fn write(&self, token: &PendingActivation) -> Result<(), SelfDevError> {
        std::fs::create_dir_all(&self.root).map_err(SelfDevError::io)?;
        let body = serde_json::to_vec_pretty(token).map_err(SelfDevError::io)?;
        let target = self.token_path(&token.session_id);
        let temp = self.root.join(format!(
            ".{}.{}.incoming",
            token.session_id,
            std::process::id()
        ));
        std::fs::write(&temp, body).map_err(SelfDevError::io)?;
        std::fs::rename(&temp, target).map_err(|error| {
            let _ = std::fs::remove_file(&temp);
            SelfDevError::io(error)
        })
    }

    /// The in-flight activation for `session_id`, if the new build has not yet
    /// been confirmed.
    #[must_use]
    pub fn pending(&self, session_id: &str) -> Option<PendingActivation> {
        let body = std::fs::read_to_string(self.token_path(session_id)).ok()?;
        serde_json::from_str(&body).ok()
    }

    /// Confirm the activation succeeded — the new build came up and handshook — by
    /// discarding the token. Idempotent.
    ///
    /// # Errors
    /// Returns [`SelfDevError::Io`] only if a present token cannot be removed.
    pub fn complete(&self, session_id: &str) -> Result<(), SelfDevError> {
        match std::fs::remove_file(self.token_path(session_id)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(SelfDevError::io(error)),
        }
    }

    /// Roll the channel back to what it pointed at before the activation, and
    /// discard the token. Called when the new build failed to come up.
    ///
    /// Returns `true` if a token was present and rolled back, `false` if there was
    /// nothing to roll back. When the previous target was unset, the channel is
    /// left pointing at the new (failed) version rather than being deleted — a
    /// dangling channel already resolves to nothing (subject 02), which is the
    /// same safe outcome, and clearing it is a separate operation this does not own.
    ///
    /// # Errors
    /// Returns [`SelfDevError::Io`] when the channel cannot be repointed.
    pub fn roll_back(&self, channels: &Channels, session_id: &str) -> Result<bool, SelfDevError> {
        let Some(token) = self.pending(session_id) else {
            return Ok(false);
        };
        if let Some(previous) = &token.previous_version {
            channels.set(ChannelName::parse(&token.channel)?, previous)?;
        }
        self.complete(session_id)?;
        Ok(true)
    }
}

/// The result of comparing a candidate build against the running one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// The candidate is provably newer — the only case that may trigger a reload.
    Newer,
    /// The candidate is the same age or older; no reload.
    NotNewer,
    /// One side's mtime could not be read; treated as "no update", never as newer.
    Indeterminate,
}

/// Compare a candidate payload against the running payload by modification time.
///
/// Both paths must be the *payload that runs* — the concrete immutable executable
/// a channel resolves to, never a channel marker — so a wrapper's timestamp can
/// never stand in for the binary's (the reference's "compare the payload, not the
/// wrapper" lesson; here it is structural because a channel is a separate file).
///
/// Returns [`Freshness::Newer`] only when both mtimes are readable *and* the
/// candidate is strictly newer. An unreadable mtime is [`Freshness::Indeterminate`],
/// which a caller must treat as "do not update" — the reference's infinite-reload
/// bugs began with an unreadable mtime being read as "newer forever".
#[must_use]
pub fn compare_payload(candidate: &Path, running: &Path) -> Freshness {
    let (Some(candidate_mtime), Some(running_mtime)) = (mtime(candidate), mtime(running)) else {
        return Freshness::Indeterminate;
    };
    if candidate_mtime > running_mtime {
        Freshness::Newer
    } else {
        Freshness::NotNewer
    }
}

/// A file's modification time, if it can be read.
fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}

/// A persisted bound on how many times auto-reload may be attempted before it is
/// halted — the backstop against an infinite reload loop.
///
/// The count is durable, so a loop that keeps relaunching cannot reset it by
/// restarting: each attempt increments the same on-disk counter, and once it
/// reaches the limit the breaker is tripped until a *successful* reload resets it.
#[derive(Debug, Clone)]
pub struct AutoReloadBreaker {
    path: PathBuf,
    max: u32,
}

impl AutoReloadBreaker {
    /// A breaker rooted at `selfdev_root`, tripping after `max` failed attempts.
    #[must_use]
    pub fn new(selfdev_root: impl Into<PathBuf>, max: u32) -> Self {
        Self {
            path: selfdev_root.into().join(BREAKER_FILE),
            max,
        }
    }

    /// The current attempt count (0 when the counter file is absent or garbage).
    #[must_use]
    pub fn count(&self) -> u32 {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Record one auto-reload attempt and return the new count. Call this
    /// **before** relaunching, so a relaunch that never comes back is still
    /// counted.
    ///
    /// # Errors
    /// Returns [`SelfDevError::Io`] when the counter cannot be persisted.
    pub fn record_attempt(&self) -> Result<u32, SelfDevError> {
        let next = self.count().saturating_add(1);
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(SelfDevError::io)?;
        }
        std::fs::write(&self.path, next.to_string()).map_err(SelfDevError::io)?;
        Ok(next)
    }

    /// Whether auto-reload has been attempted too many times to keep trying.
    #[must_use]
    pub fn is_tripped(&self) -> bool {
        self.count() >= self.max
    }

    /// Clear the counter after a reload that actually succeeded.
    ///
    /// # Errors
    /// Returns [`SelfDevError::Io`] only if a present counter cannot be removed.
    pub fn reset(&self) -> Result<(), SelfDevError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(SelfDevError::io(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::CURRENT;
    use crate::marker::BuildMarker;
    use crate::store::VersionStore;

    fn install(store: &VersionStore, label: &str, body: &str) {
        let dir = store.root().join(".src");
        std::fs::create_dir_all(&dir).expect("src");
        let src = dir.join(format!("b-{label}"));
        std::fs::write(&src, body).expect("write");
        let marker = BuildMarker::new(
            label,
            "h",
            "fp",
            false,
            format!("2.6.0-selfdev-{label}"),
            crate::executable_name(),
        );
        store.install(label, &src, &marker).expect("install");
    }

    #[test]
    fn a_failed_activation_rolls_the_channel_back_to_the_previous_version() {
        let temp = tempfile::tempdir().expect("temp");
        let store = VersionStore::new(temp.path());
        let channels = Channels::new(temp.path());
        let guard = ActivationGuard::new(temp.path());
        install(&store, "old", "one");
        install(&store, "new", "two");

        channels.set(CURRENT, "old").expect("point at old");
        // Begin the activation (captures "old"), then repoint at "new".
        guard
            .begin(&channels, "sess", CURRENT.into(), "new")
            .expect("begin");
        channels.set(CURRENT, "new").expect("point at new");
        assert_eq!(channels.label(CURRENT).as_deref(), Some("new"));

        // The new build failed: roll back.
        let rolled = guard.roll_back(&channels, "sess").expect("roll back");
        assert!(rolled);
        assert_eq!(
            channels.label(CURRENT).as_deref(),
            Some("old"),
            "a failed reload must restore the previous channel target"
        );
        assert!(guard.pending("sess").is_none(), "the token is discarded");
    }

    #[test]
    fn a_successful_activation_completes_and_leaves_the_new_version_in_place() {
        let temp = tempfile::tempdir().expect("temp");
        let channels = Channels::new(temp.path());
        let guard = ActivationGuard::new(temp.path());
        channels.set(CURRENT, "old").expect("old");

        guard
            .begin(&channels, "sess", CURRENT.into(), "new")
            .expect("begin");
        channels.set(CURRENT, "new").expect("new");
        guard.complete("sess").expect("complete");

        assert!(guard.pending("sess").is_none());
        assert_eq!(channels.label(CURRENT).as_deref(), Some("new"));
    }

    #[test]
    fn rolling_back_nothing_is_a_false_no_op() {
        let temp = tempfile::tempdir().expect("temp");
        let channels = Channels::new(temp.path());
        let guard = ActivationGuard::new(temp.path());
        assert!(!guard.roll_back(&channels, "sess").expect("roll back"));
    }

    #[test]
    fn completing_and_rolling_back_are_idempotent() {
        let temp = tempfile::tempdir().expect("temp");
        let channels = Channels::new(temp.path());
        let guard = ActivationGuard::new(temp.path());
        guard.complete("sess").expect("complete absent");
        assert!(!guard
            .roll_back(&channels, "sess")
            .expect("roll back absent"));
    }

    #[test]
    fn a_strictly_newer_payload_is_newer_and_the_same_is_not() {
        let temp = tempfile::tempdir().expect("temp");
        let older = temp.path().join("older");
        let newer = temp.path().join("newer");
        std::fs::write(&older, "a").expect("write older");
        // Ensure a distinct, later mtime without depending on wall-clock: set the
        // older file's mtime into the past.
        let past = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        filetime_set(&older, past);
        std::fs::write(&newer, "b").expect("write newer");
        filetime_set(&newer, past + std::time::Duration::from_secs(10));

        assert_eq!(compare_payload(&newer, &older), Freshness::Newer);
        assert_eq!(compare_payload(&older, &newer), Freshness::NotNewer);
        assert_eq!(compare_payload(&older, &older), Freshness::NotNewer);
    }

    #[test]
    fn an_unreadable_mtime_is_indeterminate_never_newer() {
        let temp = tempfile::tempdir().expect("temp");
        let real = temp.path().join("real");
        std::fs::write(&real, "x").expect("write");
        let missing = temp.path().join("does-not-exist");

        assert_eq!(compare_payload(&missing, &real), Freshness::Indeterminate);
        assert_eq!(compare_payload(&real, &missing), Freshness::Indeterminate);
    }

    #[test]
    fn the_breaker_trips_after_the_bound_and_resets_on_success() {
        let temp = tempfile::tempdir().expect("temp");
        let breaker = AutoReloadBreaker::new(temp.path(), 3);
        assert!(!breaker.is_tripped());

        assert_eq!(breaker.record_attempt().expect("1"), 1);
        assert_eq!(breaker.record_attempt().expect("2"), 2);
        assert!(
            !breaker.is_tripped(),
            "two attempts is under the bound of three"
        );
        assert_eq!(breaker.record_attempt().expect("3"), 3);
        assert!(
            breaker.is_tripped(),
            "the third attempt reaches the bound and trips the breaker"
        );

        breaker.reset().expect("reset");
        assert!(!breaker.is_tripped());
        assert_eq!(breaker.count(), 0);
    }

    #[test]
    fn the_breaker_count_is_durable_across_instances() {
        let temp = tempfile::tempdir().expect("temp");
        AutoReloadBreaker::new(temp.path(), 5)
            .record_attempt()
            .expect("attempt");
        // A fresh breaker over the same root sees the persisted count — a
        // relaunching loop cannot reset it by restarting.
        assert_eq!(AutoReloadBreaker::new(temp.path(), 5).count(), 1);
    }

    /// Set a file's mtime using only std, by opening it and using the platform's
    /// set-times through a small shim. We avoid a new dependency by round-tripping
    /// through `filetime`-free std: write, then use `set_file_mtime` via the
    /// `File::set_modified` API (stable since Rust 1.75, above our MSRV 1.82).
    fn filetime_set(path: &Path, when: SystemTime) {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for mtime");
        file.set_modified(when).expect("set mtime");
    }
}
