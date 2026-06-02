//! Provider quota window tracking and wait/resume scheduling for Unshackled.
//!
//! Owns provider quota window tracking, reset timers, wait/resume scheduling,
//! unattended-resume policy checks, and persistence of paused harness runs.
//!
//! This crate must not own provider transport, permission decisions, tool
//! execution, or terminal UI; it coordinates with the harness, which keeps the
//! committed state and plan authoritative across a pause.
#![forbid(unsafe_code)]
