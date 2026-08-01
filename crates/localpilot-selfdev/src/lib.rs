//! Building LocalPilot from its own source, and swapping onto the result.
//!
//! The crate is a stack of primitives, each useful on its own:
//!
//! 1. [`SourceState`] — what the working tree *is*, as one comparable value.
//! 2. [`builder`] — an isolated, identity-carrying build of that tree.
//!
//! Nothing here decides *whether* to reload. That is a policy question for the
//! caller; this crate only makes each step safe to take.
#![forbid(unsafe_code)]

mod builder;
mod error;
mod git;
mod paths;
mod source;

pub use builder::{
    build, default_target_dir, plan, BuildOptions, BuildPlan, Built, ENV_GIT_HASH,
    ENV_SOURCE_FINGERPRINT, ENV_VERSION, SELFDEV_PACKAGE, SELFDEV_PROFILE, TOOL,
};
pub use error::SelfDevError;
pub use paths::default_root;
pub use source::SourceState;
