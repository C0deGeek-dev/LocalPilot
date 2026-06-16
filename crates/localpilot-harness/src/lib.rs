//! Rule-enforced agent workflow and the shared session runtime.
//!
//! Owns the agent-mode conversational loop (the shared loop both operating modes
//! use), context compaction, the `brief.md` / `PROGRESS.md` document model, and
//! the harness rule engine. Project files are the source of truth; the rule
//! engine layers on top of the permission engine and never bypasses it.
#![forbid(unsafe_code)]

mod brief;
mod compaction;
mod decisions;
mod error;
mod evidence;
mod hooks;
mod planning;
mod progress;
mod quality;
mod resume;
mod rules;
mod session;
mod summarizer;
mod system_prompt;
mod worker;

pub use brief::Brief;
pub use compaction::{
    compact, compact_with_summary, estimate_tokens, CompactionMetadata, CompactionMode,
};
pub use decisions::{today, Decision, Decisions};
pub use error::HarnessError;
pub use evidence::{CallOutcome, CallRecord, EvidenceLedger, PermissionVerdict};
pub use hooks::{ContextHook, HookEvent, HookFabric, SessionObserver};
pub use planning::{run_intake, run_plan, INTAKE_PROMPT, PLANNER_PROMPT};
pub use progress::{Progress, Step};
pub use quality::{
    program_on_path, propose_gate, ratify_gate, render_check, summarize_proposal, CheckOutcome,
    CheckRunner, CheckStatus, GateRatification, ProposedCheck, ToolchainProfile,
    QUALITY_CHECK_TOOL,
};
pub use resume::{resume_one_step, resume_one_step_with_events, ResumeOutcome, QUOTA_PAUSE_KEY};
pub use rules::{trigger_for_cadence, Rule, RuleContext, RuleEngine, Trigger, Verdict};
pub use session::{
    effective_context_limit, ManualCompaction, PlanStep, RuntimeEvent, SessionConfig,
    SessionRuntime, SteerQueue, StopReason,
};
pub use summarizer::{FallbackReason, ProviderSummarizer, Summarizer, SummarizerTuning};
pub use system_prompt::agent_system_prompt;
// Part of the public `RuntimeEvent::Recovery` payload, so consumers can match it.
pub use localpilot_recovery::ModelHealth;
pub use worker::{
    decide_step, evaluate_completion, select_next_step, AttemptResult, CompletionDecision,
    CompletionInputs, StepAction, StepDecision, StepLoop, StepTrace,
};
