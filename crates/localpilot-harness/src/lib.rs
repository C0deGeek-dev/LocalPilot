//! Rule-enforced agent workflow and the shared session runtime.
//!
//! Owns the agent-mode conversational loop (the shared loop both operating modes
//! use), context compaction, the `brief.md` / `PROGRESS.md` document model, and
//! the harness rule engine. Project files are the source of truth; the rule
//! engine layers on top of the permission engine and never bypasses it.
#![forbid(unsafe_code)]

pub mod agent_run;
mod binding;
mod brief;
mod claim;
mod compaction;
mod decisions;
mod dispatch_gate;
mod elision;
mod error;
mod evidence;
mod guidance;
mod handoff;
mod hooks;
mod incognito;
mod intake;
mod judge;
mod launch_targets;
mod lessons;
mod paths_in_play;
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
mod step_sessions;
mod summarizer;
mod system_prompt;
mod verify_target;
mod worker;
mod workspace_state;

pub use binding::BriefRevision;
pub use brief::Brief;
pub use compaction::{
    compact, compact_with_summary, estimate_tokens, CompactionMetadata, CompactionMode,
};
pub use decisions::{today, Decision, Decisions};
pub use error::HarnessError;
pub use evidence::{CallOutcome, CallRecord, EvidenceLedger, PermissionVerdict};
pub use guidance::{
    assess_guidance, guidance_score, DecisionAxis, GuidanceAssessment, GUIDANCE_PROMPT,
};
pub use handoff::{
    check_handoff, evaluate_resume, write_handoff, Handoff, HandoffHeader, HandoffSummary,
    ResumeEnv, ResumeFinding, ResumeReport, HANDOFF_SCHEMA,
};
pub use hooks::{ContextContribution, ContextHook, HookFabric};
pub use incognito::IncognitoLedger;
pub use intake::{
    append_intake_record, draft_brief, draft_with_answers, persist_approved, question_for,
    revise_brief, Approval, BriefDraft, BriefStage, DraftOutcome, GuidanceParams, GuidanceRecord,
    RetryTarget, StageOutcome,
};
pub use judge::{judge_ranking_selftest_live, judge_score_live};
pub use lessons::{Lesson, Lessons};
pub use paths_in_play::PathsInPlay;
pub use planning::{run_intake, run_plan, INTAKE_PROMPT, PLANNER_PROMPT};
pub use progress::{Progress, Step};
pub use project_analysis::{
    analyze_project, register_project_analysis_context, ProjectAnalysis, ProjectAnalysisContext,
};
pub use project_instructions::{register_project_instructions_context, ProjectInstructionsContext};
pub use quality::{
    program_on_path, propose_gate, ratify_gate, render_check, summarize_proposal, CheckOutcome,
    CheckRunner, CheckSeverity, CheckStatus, GateRatification, ProposedCheck, ToolchainProfile,
    QUALITY_CHECK_TOOL,
};
pub use resume::{resume_one_step, resume_one_step_with_events, ResumeOutcome, QUOTA_PAUSE_KEY};
pub use retrospective::{
    append_lessons, run_and_record, run_retrospective, Retrospective, RETROSPECTIVE_PROMPT,
};
pub use rules::{trigger_for_cadence, Rule, RuleContext, RuleEngine, RuleVerdict, Trigger};
pub use scorecard::{
    build_scorecard, extract_process, single_run_discipline, speed_from_events, RunInputs,
    SchemaValidator,
};
pub use step_sessions::STEP_SESSIONS_KEY;
// The shared eval surface (scorecard contract, discipline metrics, blinded
// judge, ablation) re-exported so consumers keep one import path.
pub use localx_eval_core::{
    ablation_matrix, attribute, blind, cohens_kappa, complexity_delta_in_diff, composite_score,
    feature_signal, judge_prompt, mean_std, parse_judge_block, parse_preference, preference_prompt,
    rank, resolve_preference, signal_value, tests_added_in_diff, AblationArm, AttributionRow,
    BlindedPair, CompositeOutcome, DiffStat, DisciplineMetrics, FeatureToggles, Judge, JudgeBlock,
    JudgeCache, JudgeError, JudgeInput, Preferred, ProcessBlock, QualityBlock, RankingFixture,
    RankingTrust, ResultsBlock, Scorecard, SpeedBlock, RANKING_FIXTURES, RUBRIC, SCORECARD_SCHEMA,
};
pub use session::{
    effective_context_limit, ManualCompaction, PlanStep, QuiesceSignal, RuntimeEvent,
    SessionConfig, SessionRecovery, SessionRuntime, SoftInterrupt, SoftInterruptSource, SteerQueue,
    StopReason, SwitchError, SwitchOutcome, TurnHandoff,
};
pub use summarizer::{FallbackReason, ProviderSummarizer, Summarizer, SummarizerTuning};
pub use system_prompt::{
    agent_system_prompt, assignment_contract, swarm_coordinator_directive, SwarmDepth,
};
pub use verify_target::{detect_verify_command, resolve_verify_check, VERIFY_CHECK_NAME};
// Part of the public `RuntimeEvent::Recovery` payload, so consumers can match it.
pub use localpilot_recovery::ModelHealth;
pub use worker::{
    decide_step, evaluate_completion, select_next_step, AttemptResult, CompletionDecision,
    CompletionInputs, StepAction, StepDecision, StepLoop, StepTrace,
};
pub use workspace_state::{
    adopt_plan, inspect, resumable, AdoptError, DocumentState, InterruptedRun, NotResumable,
    OperationLiveness, OperationState, WorkspaceInputs, WorkspaceState,
};
