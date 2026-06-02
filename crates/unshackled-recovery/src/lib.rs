//! Bad-output detection and recovery for Unshackled.
//!
//! Owns bad-output detection, repeated-token loop detection, the stream
//! abort/retry ladder, provider degradation state, and recovery diagnostics.
//! Recovery must prefer stopping safely over continuing with corrupted context.
//!
//! This crate must not own provider transport, tool execution, permission
//! decisions, or terminal UI.
#![forbid(unsafe_code)]
