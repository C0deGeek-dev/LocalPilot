//! Execution policy and sandbox for LocalPilot.
//!
//! Owns the workspace path boundary, per-OS command risk classification, the
//! permission engine and its three profiles (`default`/`relaxed`/`bypass`), and
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
pub use path::Workspace;
pub use permission::{
    Approver, Decision, Effect, Interactivity, PermissionEngine, PermissionRequest, Profile,
    ScriptedApprover,
};
pub use secret_path::is_secret_like;
