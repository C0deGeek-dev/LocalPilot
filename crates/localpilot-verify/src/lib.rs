//! Deterministic verification of executed tool calls against their contracts.
//!
//! The runtime already controls *whether* a tool may run (the permission
//! engine). This crate adds the missing half: after a call runs, did it actually
//! do what its contract promised? A [`Verifier`] turns a tool's recorded result
//! into a [`Verdict`] — `Verified`, `Unverified`, or `Failed` — so the loop can
//! refuse a "success" claim that no postcondition or observation supports.
//!
//! Verification is **deterministic-first**: [`DeterministicVerifier`] evaluates
//! the contract's postconditions with no model in the loop. A model-critic
//! verifier is a future drop-in behind the same [`Verifier`] trait (the same
//! deterministic-vs-model split LocalMind uses for session extraction). An
//! effect a contract marks `Unverifiable` is recorded as `Unverified`, **never**
//! as success.
#![forbid(unsafe_code)]

mod observation;
mod verdict;
mod verifier;

pub use observation::{Observation, Trust};
pub use verdict::Verdict;
pub use verifier::{DeterministicVerifier, VerificationInput, Verifier};
