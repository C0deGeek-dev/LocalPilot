//! Wire the four self-improvement stages into one human-gated loop, without
//! merging their crates.
//!
//! LocalPilot already ships four stages that each work alone: the read-only
//! find (`localpilot-selfreview`), the scope-confined, human-gated source
//! mutation (`localpilot-patchgen`), and the binary lifecycle — build, gauntlet,
//! immutable store, reload (`localpilot-selfdev`). This crate is the **thin
//! orchestrator** that sequences them:
//!
//! ```text
//! review  ─▶  propose  ─▶  [ human ApprovalToken gate ]  ─▶  promote  ─▶  build+gauntlet  ─▶  reload
//! ```
//!
//! It only sequences and surfaces state. It does **not** merge the crates — source
//! mutation (with a human-merge gate) and the binary lifecycle (with a build
//! gauntlet and rollback breaker) are different concerns with different blast
//! radii — and it **never mints the approval token**. The unattended autonomous
//! loop stays deferred (ADR-0128); this loop advances one human-driven step at a
//! time. See the [`contract`] for the states and gate, the [`orchestrator`] for
//! the sequencing, and [`guardrails`] for the properties that hold by construction.
#![forbid(unsafe_code)]

mod contract;
mod guardrails;
mod orchestrator;

pub use contract::{
    cross_gate, unattended_next, ApprovalToken, AwaitingApproval, LoopError, PatchError, Stage,
};
pub use orchestrator::{
    BuildRecord, LoopState, Orchestrator, Proposed, SelfDevRunner, SelfDevStage, StageError,
};
