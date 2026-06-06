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
mod secret;
mod tool;
mod usage;

pub use error::CoreError;
pub use id::{MessageId, SessionId, ToolUseId, TurnId};
pub use message::{ContentBlock, Message, MessageMetadata, Role};
pub use secret::Secret;
pub use tool::{ToolCall, ToolResult};
pub use usage::{TokenUsage, UsageSummary};
