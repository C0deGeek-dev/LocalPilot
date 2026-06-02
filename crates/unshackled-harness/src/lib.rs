//! Rule-enforced agent workflow contracts and the shared session runtime.
//!
//! Owns the agent-mode conversational loop (the shared loop both operating modes
//! use), context compaction, and the brief/progress data model that the harness
//! mode builds on.
#![forbid(unsafe_code)]

mod compaction;
mod session;

pub use compaction::{compact, estimate_tokens};
pub use session::{RuntimeEvent, SessionConfig, SessionRuntime, StopReason};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Brief {
    pub name: String,
    pub summary: String,
    pub requirements: Vec<String>,
    pub constraints: Vec<String>,
    pub non_goals: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Progress {
    pub name: String,
    pub branch: String,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub number: usize,
    pub description: String,
    pub status: StepStatus,
    pub attempts: usize,
    pub head_hash: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepStatus {
    Pending,
    Done,
}
