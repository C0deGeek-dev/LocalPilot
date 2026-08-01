//! Building LocalPilot from its own source, and swapping onto the result.
//!
//! The crate is a stack of primitives, each useful on its own:
//!
//! 1. [`SourceState`] — what the working tree *is*, as one comparable value.
//! 2. [`build`] — an isolated, identity-carrying build of that tree.
//! 3. [`VersionStore`] — every build in its own immutable directory.
//! 4. [`Channels`] — marker-file pointers naming which stored version runs.
//! 5. [`vet`] — the gauntlet a candidate passes before a channel points at it.
//! 6. [`ReloadStore`] / [`relaunch`] — swap onto a build and continue the session.
//!
//! Nothing here decides *whether* to reload. That is a policy question for the
//! caller; this crate only makes each step safe to take.
#![forbid(unsafe_code)]

mod builder;
mod channel;
mod error;
mod gauntlet;
mod git;
mod marker;
mod paths;
mod reload;
mod rollback;
mod source;
mod store;

pub use builder::{
    build, default_target_dir, executable_name, plan, BuildOptions, BuildPlan, Built, ENV_GIT_HASH,
    ENV_SOURCE_FINGERPRINT, ENV_VERSION, SELFDEV_PACKAGE, SELFDEV_PROFILE, TOOL,
};
pub use channel::{Channel, ChannelName, Channels, CURRENT, SLOW, STABLE};
pub use error::SelfDevError;
pub use gauntlet::{
    check_fresh, check_identity, read_reported_identity, smoke_handshake, vet, write_smoke_config,
    ReportedIdentity, DEFAULT_HANDSHAKE_TIMEOUT,
};
pub use marker::{BuildMarker, BUILD_MARKER_VERSION};
pub use paths::default_root;
pub use reload::{
    perform_reload, relaunch, relaunch_plan, stage_reload, RelaunchPlan, ReloadIntent,
    ReloadRequest, ReloadStore, RELOAD_INTENT_VERSION,
};
pub use rollback::{
    compare_payload, ActivationGuard, AutoReloadBreaker, Freshness, PendingActivation,
};
pub use source::SourceState;
pub use store::{StoredVersion, VersionStore};
