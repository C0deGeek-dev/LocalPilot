//! Tool system for LocalPilot.
//!
//! Tools are the only path from model output to local side effects. Every call
//! goes through one registry that validates input against a generated schema,
//! authorizes each effect through the permission engine, executes, and redacts
//! the result. This crate owns local side effects; permission decisions live in
//! `localpilot-sandbox`, and the registry never bypasses them.
#![forbid(unsafe_code)]

mod broker;
mod builtins;
mod builtins_background;
mod builtins_shell;
mod catalog;
mod contract;
mod error;
mod registry;
mod tool;
mod validate;

pub use broker::{
    learned_boost, resolve, Broker, BrokerConfig, Locator, Resolution, ResolutionRecord,
    RevealOutcome, ToolLoad, ToolSearch, DEFAULT_GRADUATION_THRESHOLD, TOOL_LOAD, TOOL_SEARCH,
};
pub use builtins::{
    ApplyPatch, EditFile, Fetch, GitCommit, GitStatus, ListFiles, ReadFile, ReadToolOutput,
    ReplaceInFile, SearchText, WriteFile,
};
pub use builtins_background::{BackgroundProcesses, ProcStatus, RunBackground};
pub use builtins_shell::RunShell;
pub use catalog::{
    fingerprint, Catalog, CatalogDelta, CatalogEntry, DeprecationOverlay, ToolSource,
};
pub use contract::{
    string_arg, Confirmation, ContentExpectation, FailureMode, Idempotency, PathEffectKind,
    Postcondition, Precondition, RetryPolicy, Reversibility, SideEffectClass, StatePredicate,
    ToolContract, ToolExample, ToolVersion, VerificationMethod,
};
pub use error::ToolError;
pub use registry::ToolRegistry;
pub use tool::{GateVerdict, OutputRetention, Tool, ToolContext, ToolGate, ToolOutput};
pub use validate::{
    is_input_valid, required_fields_present, tool_input_issues, MalformedClass, SchemaIssue,
};
