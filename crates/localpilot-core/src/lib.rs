//! Core domain types for LocalPilot.
//!
//! This crate is the provider-neutral, UI-neutral heart of the workspace: the
//! message and content model, normalized tool call/result types, usage
//! accounting, strongly-typed identifiers, the secret wrapper, and the core
//! error type. It must stay free of HTTP clients, terminal UI, and
//! provider-specific names beyond generic enum variants.
#![forbid(unsafe_code)]

mod error;
mod id;
mod message;
mod search;
mod secret;
mod summary;
mod text;
mod tool;
mod usage;

pub use error::CoreError;
pub use id::{EventId, MessageId, SessionId, ToolUseId, TurnId};
pub use message::{ContentBlock, Message, MessageMetadata, Role};
pub use search::{word_overlap, Locator};
pub use secret::Secret;
pub use summary::{
    StructuredSummary, SummaryBudget, SummarySection, SummarySectionKind, SummarySource,
    SummarySourceKind, STRUCTURED_SUMMARY_SCHEMA_VERSION,
};
pub use text::{collapse_whitespace, one_line, truncate_collapsed, SUMMARY_CHARS};
pub use tool::{ToolCall, ToolResult};
pub use usage::{TokenUsage, UsageSummary};
