//! Rule-enforced agent workflow and the shared session runtime.
//!
//! Owns the agent-mode conversational loop (the shared loop both operating modes
//! use), context compaction, the `brief.md` / `PROGRESS.md` document model, and
//! the harness rule engine. Project files are the source of truth; the rule
//! engine layers on top of the permission engine and never bypasses it.
#![forbid(unsafe_code)]

mod ablation;
mod brief;
mod claim;
mod compaction;
mod decisions;
mod discipline;
mod dispatch_gate;
mod error;
mod evidence;
mod handoff;
mod hooks;
mod judge;
mod launch_targets;
mod lessons;
mod planning;
mod precondition;
mod progress;
mod project_analysis;
mod project_instructions;
mod quality;
mod resume;
mod retrospective;
mod rules;
mod scorecard;
mod session;
mod summarizer;
mod system_prompt;
mod verify_target;
mod worker;

pub use ablation::{
    ablation_matrix, attribute, composite_score, feature_signal, mean_std, rank, signal_value,
    AblationArm, AttributionRow, CompositeOutcome, FeatureToggles,
};
pub use brief::Brief;
pub use compaction::{
    compact, compact_with_summary, estimate_tokens, CompactionMetadata, CompactionMode,
};
pub use decisions::{today, Decision, Decisions};
pub use discipline::DisciplineMetrics;
pub use error::HarnessError;
pub use evidence::{CallOutcome, CallRecord, EvidenceLedger, PermissionVerdict};
pub use handoff::{
    check_handoff, evaluate_resume, write_handoff, Handoff, HandoffHeader, HandoffSummary,
    ResumeEnv, ResumeFinding, ResumeReport, HANDOFF_SCHEMA,
};
pub use hooks::{ContextContribution, ContextHook, HookEvent, HookFabric, SessionObserver};
pub use judge::{
    blind, cohens_kappa, judge_prompt, parse_judge_block, parse_preference, preference_prompt,
    resolve_preference, BlindedPair, Judge, JudgeBlock, JudgeCache, JudgeError, JudgeInput,
    Preferred, RankingFixture, RankingTrust, RANKING_FIXTURES, RUBRIC,
};
pub use lessons::{Lesson, Lessons};
pub use planning::{run_intake, run_plan, INTAKE_PROMPT, PLANNER_PROMPT};
pub use progress::{Progress, Step};
pub use project_analysis::{
    analyze_project, register_project_analysis_context, ProjectAnalysis, ProjectAnalysisContext,
};
pub use project_instructions::{register_project_instructions_context, ProjectInstructionsContext};
pub use quality::{
    program_on_path, propose_gate, ratify_gate, render_check, summarize_proposal, CheckOutcome,
    CheckRunner, CheckStatus, GateRatification, ProposedCheck, ToolchainProfile,
    QUALITY_CHECK_TOOL,
};
pub use resume::{resume_one_step, resume_one_step_with_events, ResumeOutcome, QUOTA_PAUSE_KEY};
pub use retrospective::{run_and_record, run_retrospective, Retrospective, RETROSPECTIVE_PROMPT};
pub use rules::{trigger_for_cadence, Rule, RuleContext, RuleEngine, RuleVerdict, Trigger};
pub use scorecard::{
    build_scorecard, complexity_delta_in_diff, extract_process, single_run_discipline,
    tests_added_in_diff, DiffStat, ProcessBlock, QualityBlock, ResultsBlock, RunInputs,
    SchemaValidator, Scorecard, SpeedBlock, SCORECARD_SCHEMA,
};
pub use session::{
    effective_context_limit, ManualCompaction, PlanStep, RuntimeEvent, SessionConfig,
    SessionRuntime, SteerQueue, StopReason, SwitchError, SwitchOutcome, TurnHandoff,
};
pub use summarizer::{FallbackReason, ProviderSummarizer, Summarizer, SummarizerTuning};
pub use system_prompt::agent_system_prompt;
pub use verify_target::{detect_verify_command, resolve_verify_check, VERIFY_CHECK_NAME};
// Part of the public `RuntimeEvent::Recovery` payload, so consumers can match it.
pub use localpilot_recovery::ModelHealth;
pub use worker::{
    decide_step, evaluate_completion, select_next_step, AttemptResult, CompletionDecision,
    CompletionInputs, StepAction, StepDecision, StepLoop, StepTrace,
};
