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
mod builtins_ask;
mod builtins_background;
mod builtins_shell;
mod builtins_swarm;
mod catalog;
mod contract;
mod error;
mod registry;
mod repair;
mod schema_intent;
mod tool;
mod validate;

pub use broker::{
    describes_documentation, learned_boost, resolve, Broker, BrokerConfig, Locator, Resolution,
    ResolutionRecord, RevealOutcome, ToolLoad, ToolSearch, DEFAULT_GRADUATION_THRESHOLD, TOOL_LOAD,
    TOOL_SEARCH,
};
pub use builtins::{
    ApplyPatch, EditFile, Fetch, GitCommit, GitStatus, ListFiles, ReadFile, ReadToolOutput,
    ReplaceInFile, SearchText, WriteFile,
};
pub use builtins_ask::{AskUser, ASK_USER};
pub use builtins_background::{BackgroundProcesses, ProcStatus, RunBackground};
pub use builtins_shell::RunShell;
pub use builtins_swarm::{Swarm, SWARM};
pub use catalog::{
    fingerprint, Catalog, CatalogDelta, CatalogEntry, DeprecationOverlay, ToolSource,
};
pub use contract::{
    string_arg, Confirmation, ContentExpectation, FailureMode, Idempotency, PathEffectKind,
    Postcondition, Precondition, RetryPolicy, Reversibility, SideEffectClass, StatePredicate,
    ToolContract, ToolExample, ToolVersion, VerificationMethod,
};
pub use error::ToolError;
pub use localpilot_core::ToolOutcome;
pub use registry::ToolRegistry;
pub use repair::{
    evaluate as evaluate_tool_input, is_repair_eligible, parse_stringified_json,
    unwrap_markdown_autolink, wrap_bare_string_as_array, RepairOutcome, RepairRequest,
    ToolInputValidationResult,
};
pub use schema_intent::{field_intent, is_repair_exempt, INTENT_KEY};
pub use tool::{
    AgentHost, Audience, Delivered, Delivery, GateVerdict, OutputRetention, PeerMessage,
    PeerSummary, QuestionOption, SwarmIdentity, SwarmPeers, Tool, ToolContext, ToolGate,
    ToolOutput, UserAnswer, UserPrompter, UserQuestion,
};
pub use validate::{
    is_input_valid, readable_input_error, required_fields_present, tool_input_issues,
    MalformedClass, SchemaIssue,
};
