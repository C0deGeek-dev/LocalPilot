//! Tool system for LocalPilot.
//!
//! Tools are the only path from model output to local side effects. Every call
//! goes through one registry that validates input against a generated schema,
//! authorizes each effect through the permission engine, executes, and redacts
//! the result. This crate owns local side effects; permission decisions live in
//! `localpilot-sandbox`, and the registry never bypasses them.
#![forbid(unsafe_code)]

mod builtins;
mod error;
mod registry;
mod tool;

pub use builtins::{
    ApplyPatch, EditFile, Fetch, GitCommit, GitStatus, ListFiles, ReadFile, ReadToolOutput,
    ReplaceInFile, RunShell, SearchText, WriteFile,
};
pub use error::ToolError;
pub use registry::ToolRegistry;
pub use tool::{GateVerdict, OutputRetention, Tool, ToolContext, ToolGate, ToolOutput};
