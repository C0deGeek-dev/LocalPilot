//! Execution policy and sandbox for LocalPilot.
//!
//! Owns the workspace path boundary, per-OS command risk classification, the
//! permission engine and its profiles (`default`/`relaxed`/`readonly`/`bypass`/
//! `unrestricted`), and
//! the approval interface. This crate makes the permission decisions; it holds no
//! provider, tool-execution, or UI logic. Every tool effect must be evaluated
//! through [`PermissionEngine::decide`] — there is no path around it.
#![forbid(unsafe_code)]

mod command;
mod error;
mod path;
mod permission;
mod secret_path;

pub use command::{classify, classify_posix, classify_windows, CommandClass};
pub use error::SandboxError;
pub use path::{ScratchRoot, Workspace};
pub use permission::{
    AllowedCommand, Approver, Decision, Effect, ExactCommand, Interactivity, Lease, LeaseState,
    PermissionEngine, PermissionEngineHandle, PermissionRequest, Profile, ScriptedApprover,
};
pub use secret_path::is_secret_like;
