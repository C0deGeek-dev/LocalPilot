//! Rule-enforced agent workflow contracts.
#![forbid(unsafe_code)]

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
