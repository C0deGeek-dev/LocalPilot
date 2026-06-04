//! The discovered project quality gate (ADR-0009).
//!
//! Profiles supply the fixed, stack-neutral abstraction; discovery proposes a
//! concrete gate from the tools actually present, tagging each with its command
//! risk class. Nothing here executes or writes — ratification, execution, the
//! rule, and the act-on-findings loop are layered in by later work.

mod discovery;
mod profiles;
mod runner;

pub use discovery::{program_on_path, propose_gate, ProposedCheck};
pub use profiles::ToolchainProfile;
pub use runner::{CheckOutcome, CheckRunner, CheckStatus, QUALITY_CHECK_TOOL};
