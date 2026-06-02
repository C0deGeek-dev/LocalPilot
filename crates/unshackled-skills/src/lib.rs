//! Skill discovery and suggestion for Unshackled.
//!
//! Owns skill discovery, skill execution metadata, skill suggestion heuristics,
//! generated skill drafts, and skill permission manifests. Auto-generated skills
//! are suggestions until the user reviews and accepts them.
//!
//! This crate must not own tool execution, permission enforcement, provider
//! calls, or terminal UI.
#![forbid(unsafe_code)]
