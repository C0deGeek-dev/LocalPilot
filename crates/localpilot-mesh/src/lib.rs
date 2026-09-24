//! The pair-programming mailbox protocol, natively.
//!
//! This crate is LocalPilot's implementation of the mailbox protocol that
//! Claude Code, Codex and LocalPilot use to pair on one working tree. The
//! protocol's normative source is its written specification (version 1.0) and
//! the language-neutral conformance suite that pins it; this crate is built to
//! those, not to any other implementation's code. It claims the specification's
//! **participant profile**: it joins, reads, posts, acknowledges, hands off,
//! signs off and accepts delivery, and leaves creating and retiring sessions to
//! a full implementation.
//!
//! One journal, many conformant writers: every file this crate reads or writes
//! is the same on-disk mailbox another implementation may be writing at the
//! same moment, so the lock, append and replace protocols here are exactly the
//! specification's, not a private variant.
//!
//! - [`layout`]: where each file lives (spec L-1, L-2).
//! - [`lock`]: lock files (L-5, L-6).
//! - [`fsio`]: whole-file reads and atomic replaces (L-7, L-8).
//! - [`jsonl`]: journal lines (J-1..J-8).
//! - [`records`]: typed records that keep unknown keys (V-1, V-1a).
//! - [`session`]: resolving the active session and its protocol version
//!   (S-7, V-3).
//!
//! Library only for now: nothing in LocalPilot calls it yet. The participant
//! operations and the `localpilot mesh` command that exposes them come next,
//! and are held to the same conformance suite.
#![forbid(unsafe_code)]

mod error;
pub mod fsio;
pub mod jsonl;
pub mod layout;
pub mod lock;
pub mod records;
pub mod session;
mod timefmt;

pub use error::MeshError;
pub use layout::Mailbox;
pub use timefmt::{parse_utc, utc_now};

/// The protocol version this crate implements (spec V-2).
pub const PROTOCOL: &str = "1.0";

/// Features this crate implements that a record may name in `requires`
/// (spec V-8). Empty in 1.0.
pub const FEATURES: &[&str] = &[];
