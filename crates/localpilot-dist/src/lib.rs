//! Version-cached distribution: what is installed, which one to run, and how a
//! new one lands on disk.
//!
//! The shape is one idea: **every version lives in its own directory**. That
//! makes switching a rename, rollback free, and an interrupted update harmless —
//! and it is the only approach that behaves the same on Windows, where a running
//! executable cannot be replaced in place.
//!
//! This crate deliberately does not download anything. It owns the on-disk
//! contract (cache layout, install marker, resolution order) so that the code
//! which *does* reach the network has something small and testable to commit
//! into.
#![forbid(unsafe_code)]

mod activate;
mod cache;
mod error;
mod install;
mod manifest;
mod resolve;
mod version;

pub use activate::{activate, bin_dir, executable_name, place};
pub use cache::{Cache, CachedVersion, InstallMarker, MARKER_VERSION};
pub use error::DistError;
pub use install::{
    download, escapes_destination, extract, find_executable, install_release, sha256_hex, verify,
};
pub use manifest::{current_target, Artifact, ReleaseManifest, MANIFEST_VERSION};
pub use resolve::{newer_installed, resolve, Reason, Resolution};
pub use version::{Channel, Version};
