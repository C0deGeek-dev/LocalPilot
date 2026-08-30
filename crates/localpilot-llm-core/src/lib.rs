//! Shared provider contract for LocalPilot's model adapters.
//!
//! This crate holds the provider-neutral surface every adapter builds on: the
//! object-safe [`ModelProvider`] trait and its declaration/capability
//! descriptors, the internal request and streaming-event models, the stable
//! error taxonomy and quota metadata, HTTP authentication helpers, and the
//! rate-limit header parsers. Provider-specific wire code lives in the adapter
//! crates that depend on this one.
#![forbid(unsafe_code)]

pub mod auth;
pub mod error;
pub mod event;
pub mod headers;
pub mod provider;
pub mod request;

pub use auth::{AccessToken, AuthProvider, GoogleAdcAuthProvider};
pub use error::{ProviderError, QuotaInfo};
pub use event::{ModelEvent, ModelEventStream};
pub use provider::{
    AuthRequirement, Capabilities, InputBlockKind, ModelProvider, ProviderDeclaration,
    ReasoningShape, SourceType, ToolCallShape,
};
pub use request::{constraint_for, ModelRequest, ReasoningEffort, ReasoningEffortHandle, ToolSpec};
