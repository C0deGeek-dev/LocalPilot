//! Rule-enforced agent workflow and the shared session runtime.
//!
//! Owns the agent-mode conversational loop (the shared loop both operating modes
//! use), context compaction, the `brief.md` / `PROGRESS.md` document model, and
//! the harness rule engine. Project files are the source of truth; the rule
//! engine layers on top of the permission engine and never bypasses it.
#![forbid(unsafe_code)]

mod brief;
mod compaction;
mod error;
mod planning;
mod progress;
mod resume;
mod rules;
mod session;
mod system_prompt;
mod worker;

pub use brief::Brief;
pub use compaction::{compact, compact_with_summary, estimate_tokens};
pub use error::HarnessError;
pub use planning::{run_intake, run_plan, INTAKE_PROMPT, PLANNER_PROMPT};
pub use progress::{Progress, Step};
pub use resume::{resume_one_step, ResumeOutcome, QUOTA_PAUSE_KEY};
pub use rules::{Rule, RuleContext, RuleEngine, Trigger, Verdict};
pub use session::{PlanStep, RuntimeEvent, SessionConfig, SessionRuntime, StopReason};
pub use system_prompt::agent_system_prompt;
// Part of the public `RuntimeEvent::Recovery` payload, so consumers can match it.
pub use unshackled_recovery::ModelHealth;
pub use worker::{
    evaluate_completion, select_next_step, AttemptResult, CompletionDecision, CompletionInputs,
    StepDecision, StepLoop, StepTrace,
};
