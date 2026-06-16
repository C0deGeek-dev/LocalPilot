//! Bad-output detection and recovery for LocalPilot.
//!
//! Owns context-aware detection of degraded model/backend states and the
//! recovery ladder that responds to them. It prefers stopping safely over
//! continuing with corrupted context: a recovered turn may continue the session,
//! but a bad or unrecovered turn may not complete a harness step.
#![forbid(unsafe_code)]

mod detect;
mod engine;

pub use detect::{
    detect, has_tool_loop, is_repeated_token_loop, is_slash_flood, BadOutputKind, StreamMonitor,
    ToolLoopDetector,
};
pub use engine::{ModelHealth, RecoveryAction, RecoveryBudget, RecoveryDiagnostic, RecoveryEngine};
