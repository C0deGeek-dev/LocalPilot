//! Local project memory store for Unshackled.
//!
//! Owns the local project memory store, retrieval ranking, opt-out and deletion
//! controls, and — only after the flat store proves useful — optional
//! entity/relation extraction.
//!
//! Memory is local-only by design: this crate must not own remote sync, network
//! transport, provider calls, terminal UI, or permission decisions.
#![forbid(unsafe_code)]
