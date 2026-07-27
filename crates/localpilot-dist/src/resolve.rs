//! Choosing which installed version to run.
//!
//! Three inputs, in order: a **pin** if one is set, else the **newest cached**
//! version, else the **running build** itself. The choice is returned with the
//! reason, because "why am I running this version" is the first question a
//! confused user asks and the answer should not require reading the cache.

use std::path::PathBuf;

use crate::cache::{Cache, CachedVersion};
use crate::version::Version;

/// Why the resolver chose what it chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// A pin is set and that version is installed.
    Pinned,
    /// A pin is set but that version is **not** installed; the pin was honoured
    /// as an instruction not to auto-upgrade, and the running build is used.
    PinnedButMissing { wanted: String },
    /// No pin; the newest cached version won.
    NewestCached,
    /// No pin and nothing cached — the build that is executing.
    Running,
    /// The newest cached version is the one already running. Nothing to switch
    /// to, and nothing is stale.
    SameAsRunning,
    /// A cached version exists but is older than the running build, so the
    /// running build wins. Prevents a stale cache silently downgrading someone
    /// who installed a newer build from source.
    RunningIsNewer,
}

impl Reason {
    /// A one-line explanation suitable for `doctor` or a `--verbose` line.
    #[must_use]
    pub fn explain(&self) -> String {
        match self {
            Reason::Pinned => "pinned".to_string(),
            Reason::PinnedButMissing { wanted } => {
                format!("pinned to {wanted}, which is not installed; running this build instead")
            }
            Reason::NewestCached => "newest installed version".to_string(),
            Reason::Running => "no installed versions; running this build".to_string(),
            Reason::SameAsRunning => "the installed version matches this build".to_string(),
            Reason::RunningIsNewer => "this build is newer than anything installed".to_string(),
        }
    }
}

/// The resolver's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// The version to run.
    pub version: Version,
    /// Where its executable lives, or `None` when the answer is "the build that
    /// is already running".
    pub executable: Option<PathBuf>,
    pub reason: Reason,
}

impl Resolution {
    /// Whether the caller should hand off to another binary.
    #[must_use]
    pub fn is_handoff(&self) -> bool {
        self.executable.is_some()
    }
}

/// Choose the version to run.
///
/// `running` is the version of the build making the call. A cached version only
/// wins if it is **strictly newer** — a cache full of old versions must never
/// downgrade a fresh from-source build, which is the normal state for a
/// developer working in the repo.
#[must_use]
pub fn resolve(cache: &Cache, running: &Version) -> Resolution {
    if let Some(pinned) = cache.pin() {
        return match cache.get(&pinned) {
            Some(found) => Resolution {
                version: found.version.clone(),
                executable: Some(found.executable()),
                reason: Reason::Pinned,
            },
            None if pinned == *running => Resolution {
                version: running.clone(),
                executable: None,
                reason: Reason::Pinned,
            },
            None => Resolution {
                version: running.clone(),
                executable: None,
                reason: Reason::PinnedButMissing {
                    wanted: pinned.to_dir_name(),
                },
            },
        };
    }

    match cache.newest() {
        Some(newest) if newest.version.key() > running.key() => Resolution {
            version: newest.version.clone(),
            executable: Some(newest.executable()),
            reason: Reason::NewestCached,
        },
        Some(installed) if installed.version.key() == running.key() => Resolution {
            version: running.clone(),
            executable: None,
            reason: Reason::SameAsRunning,
        },
        Some(_) => Resolution {
            version: running.clone(),
            executable: None,
            reason: Reason::RunningIsNewer,
        },
        None => Resolution {
            version: running.clone(),
            executable: None,
            reason: Reason::Running,
        },
    }
}

/// The newest installed version strictly newer than `running`, if any — what an
/// `update` command would switch to without downloading anything.
#[must_use]
pub fn newer_installed(cache: &Cache, running: &Version) -> Option<CachedVersion> {
    cache
        .newest()
        .filter(|cached| cached.version.key() > running.key())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{InstallMarker, MARKER_VERSION};

    const EXE: &str = if cfg!(windows) { "tool.exe" } else { "tool" };

    fn install(cache: &Cache, version: &str) {
        let parsed = Version::parse(version).expect("parses");
        let staged = cache.stage(&parsed).expect("stage");
        std::fs::write(staged.join(EXE), b"binary").expect("write");
        cache
            .commit(
                &parsed,
                &staged,
                &InstallMarker {
                    marker_version: MARKER_VERSION,
                    version: version.to_string(),
                    target: "t".to_string(),
                    sha256: "0".repeat(64),
                    executable: EXE.to_string(),
                },
            )
            .expect("commit");
    }

    fn cache() -> (tempfile::TempDir, Cache) {
        let dir = tempfile::tempdir().expect("tmp");
        let cache = Cache::new(dir.path().join("tool"));
        (dir, cache)
    }

    fn v(text: &str) -> Version {
        Version::parse(text).expect("parses")
    }

    #[test]
    fn an_empty_cache_runs_the_current_build() {
        let (_dir, cache) = cache();
        let r = resolve(&cache, &v("2.5.0"));
        assert_eq!(r.reason, Reason::Running);
        assert!(!r.is_handoff(), "nothing to hand off to");
    }

    #[test]
    fn a_newer_cached_version_wins() {
        let (_dir, cache) = cache();
        install(&cache, "2.6.0");
        let r = resolve(&cache, &v("2.5.0"));
        assert_eq!(r.reason, Reason::NewestCached);
        assert_eq!(r.version, v("2.6.0"));
        assert!(r.is_handoff());
    }

    #[test]
    fn a_stale_cache_never_downgrades_a_fresh_build() {
        // The normal state for a developer: built from source at tip, with older
        // released versions sitting in the cache.
        let (_dir, cache) = cache();
        install(&cache, "2.4.0");
        install(&cache, "2.5.0");
        let r = resolve(&cache, &v("2.6.0"));
        assert_eq!(r.reason, Reason::RunningIsNewer);
        assert!(!r.is_handoff(), "must not hand off to an older binary");
    }

    #[test]
    fn a_pin_beats_a_newer_cached_version() {
        let (_dir, cache) = cache();
        install(&cache, "2.4.0");
        install(&cache, "2.6.0");
        cache.set_pin(&v("2.4.0")).expect("pin");
        let r = resolve(&cache, &v("2.5.0"));
        assert_eq!(r.reason, Reason::Pinned);
        assert_eq!(r.version, v("2.4.0"), "a pin means a pin, even downwards");
    }

    #[test]
    fn a_pin_to_the_running_build_is_honoured_without_a_handoff() {
        let (_dir, cache) = cache();
        install(&cache, "2.6.0");
        cache.set_pin(&v("2.5.0")).expect("pin");
        let r = resolve(&cache, &v("2.5.0"));
        assert_eq!(r.reason, Reason::Pinned);
        assert!(!r.is_handoff());
    }

    #[test]
    fn a_pin_to_a_missing_version_does_not_silently_upgrade() {
        let (_dir, cache) = cache();
        install(&cache, "2.6.0");
        cache.set_pin(&v("9.9.9")).expect("pin");
        let r = resolve(&cache, &v("2.5.0"));
        assert_eq!(
            r.reason,
            Reason::PinnedButMissing {
                wanted: "9.9.9".to_string()
            },
            "a pin is an instruction not to move, so a missing pin must not fall through to newest"
        );
        assert!(!r.is_handoff());
        assert!(
            r.reason.explain().contains("9.9.9"),
            "the reason names what was wanted"
        );
    }

    #[test]
    fn every_reason_explains_itself() {
        for reason in [
            Reason::Pinned,
            Reason::PinnedButMissing {
                wanted: "1.0.0".into(),
            },
            Reason::NewestCached,
            Reason::Running,
            Reason::RunningIsNewer,
        ] {
            assert!(!reason.explain().is_empty());
        }
    }

    #[test]
    fn newer_installed_reports_only_a_real_upgrade() {
        let (_dir, cache) = cache();
        install(&cache, "2.4.0");
        assert!(newer_installed(&cache, &v("2.5.0")).is_none());
        install(&cache, "2.6.0");
        assert_eq!(
            newer_installed(&cache, &v("2.5.0")).expect("some").version,
            v("2.6.0")
        );
    }
}
