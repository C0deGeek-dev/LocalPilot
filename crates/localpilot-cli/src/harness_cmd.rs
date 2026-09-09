//! `localpilot init` and `localpilot harness status`.
//!
//! Status is read-only and must work without a model provider, so it never
//! constructs the provider registry or touches the network.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use indexmap::IndexMap;
use localpilot_config::{CliOverrides, Config, ConfigPaths, RuleSeverity};
use localpilot_harness::{
    propose_gate, ratify_gate, resume_one_step_with_events, run_intake, run_plan,
    summarize_proposal, Brief, CheckOutcome, CheckStatus, RuleEngine, RuntimeEvent, SessionConfig,
    SessionRuntime, QUALITY_CHECK_TOOL, QUOTA_PAUSE_KEY,
};
use localpilot_llm::{ModelProvider, ProviderRegistry};
use localpilot_quota::{decide_resume, PausedRun, ResumeContext, ResumeDecision, ResumePolicy};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{
    Approver, Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace,
};
use localpilot_store::Store;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

const DEFAULT_CONFIG: &str = "[harness]\n\
mode = \"agent\"\n\
attempts_per_step = 3\n\
auto_commit = true\n\
# test_command = \"cargo test\"\n\n\
# Pre-brief guidance gate for `harness intake`: scores how much load-bearing\n\
# product guidance the idea contains and pauses to ask below the threshold\n\
# instead of writing a brief that encodes guesses. Opt-in.\n\
# [harness.guidance]\n\
# enabled = true\n\
# threshold = 0.7\n\
# max_questions = 5\n\n\
[context]\n\
project_analysis = true\n\n\
[docs]\n\
lookup_policy = \"evidence\"\n\n\
[permissions]\n\
profile = \"default\"\n\n\
[provider]\n\
# Configure a provider below, then uncomment `default` to point at it. Until\n\
# then `localpilot` prints this doctor report instead of launching the REPL,\n\
# and `ask`/`print`/`chat` report that no provider is configured (rather than\n\
# referencing a provider that does not exist).\n\
# default = \"local\"\n\n\
# Point base_url at your local OpenAI-compatible server (llama.cpp, Ollama,\n\
# vLLM, LM Studio, ...) and set the model it serves. For an Anthropic-compatible\n\
# endpoint, use kind = \"anthropic\" and the /v1 base URL.\n\
# [providers.local]\n\
# kind = \"openai-compatible\"\n\
# base_url = \"http://localhost:8080/v1\"\n\
# model = \"your-local-model\"\n";

const GITIGNORE_ENTRY: &str = ".localpilot/";

/// Initialize project-local harness state.
///
/// # Errors
/// Returns an error if files cannot be written or git cannot be initialized.
pub fn init(root: &Path, init_git: bool) -> anyhow::Result<String> {
    let mut created = Vec::new();

    let config_path = root.join(".localpilot.toml");
    if config_path.exists() {
        created.push(".localpilot.toml (already present, left unchanged)".to_string());
    } else {
        std::fs::write(&config_path, DEFAULT_CONFIG)?;
        created.push(".localpilot.toml".to_string());
    }

    ensure_gitignore_entry(root, &mut created)?;

    if init_git && !root.join(".git").exists() {
        let status = std::process::Command::new("git")
            .arg("init")
            .current_dir(root)
            .status()?;
        if status.success() {
            created.push("git repository".to_string());
        }
    }

    Ok(format!("initialized: {}", created.join(", ")))
}

fn ensure_gitignore_entry(root: &Path, created: &mut Vec<String>) -> anyhow::Result<()> {
    let gitignore = root.join(".gitignore");
    let existing = std::fs::read_to_string(&gitignore).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == GITIGNORE_ENTRY) {
        return Ok(());
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(GITIGNORE_ENTRY);
    updated.push('\n');
    std::fs::write(&gitignore, updated)?;
    created.push(".gitignore entry for .localpilot/".to_string());
    Ok(())
}

/// A read-only harness status snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    pub branch: Option<String>,
    pub next_step: Option<String>,
    pub completed: usize,
    pub total: usize,
    pub dirty: bool,
    pub test_command: Option<String>,
    pub default_provider: String,
    pub provider_credential_present: bool,
    /// The ratified quality-gate checks, each as `name (cadence)`.
    pub gate: Vec<String>,
    /// The lifecycle line: which typed workspace state the project is in, and
    /// the named reason when that state is a failure. Composed here from the
    /// typed state rather than re-derived, so status cannot disagree with what
    /// the execution paths enforce.
    pub lifecycle: String,
}

impl StatusReport {
    /// Render the status as deterministic text.
    #[must_use]
    pub fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        let _ = writeln!(s, "branch: {}", self.branch.as_deref().unwrap_or("(none)"));
        let _ = writeln!(
            s,
            "progress: {}/{} steps complete",
            self.completed, self.total
        );
        let _ = writeln!(
            s,
            "next step: {}",
            self.next_step.as_deref().unwrap_or("(none)")
        );
        let _ = writeln!(
            s,
            "working tree: {}",
            if self.dirty { "dirty" } else { "clean" }
        );
        let _ = writeln!(
            s,
            "test command: {}",
            self.test_command.as_deref().unwrap_or("(unset)")
        );
        let _ = writeln!(
            s,
            "quality gate: {}",
            if self.gate.is_empty() {
                "(none ratified)".to_string()
            } else {
                self.gate.join(", ")
            }
        );
        let _ = writeln!(s, "lifecycle: {}", self.lifecycle);
        let credential = if self.provider_credential_present {
            "set"
        } else {
            "not set"
        };
        let _ = writeln!(
            s,
            "provider: {} (credential {credential})",
            self.default_provider
        );
        s
    }
}

/// Gather harness status from the working directory.
///
/// # Errors
/// Returns an error if the current directory or configuration cannot be read.
pub fn gather_status(root: &Path) -> anyhow::Result<StatusReport> {
    let config = localpilot_config::load(&ConfigPaths::standard(root), &CliOverrides::default())
        .unwrap_or_else(|_| Config::default());

    // One typed inspection, shared with every execution path. The previous
    // `.ok()` chain here silently turned an unreadable or malformed plan into
    // "0/0 steps" — the same output a project with no plan at all produces.
    //
    // The interruption record comes from the store, because that is where it
    // lives; without supplying it, status could never report the paused run its
    // own documentation promises. Liveness stays `Idle`: a one-shot command
    // cannot know whether some other process is mid-run, and claiming otherwise
    // would be a guess.
    let state = localpilot_harness::inspect(localpilot_harness::WorkspaceInputs {
        root,
        liveness: localpilot_harness::OperationLiveness::Idle,
        interrupted: recorded_interruption(root),
    });
    let (next_step, completed, total) = match state.documents.progress() {
        Some(progress) => (
            progress
                .next_incomplete()
                .map(|s| format!("{}. {}", s.number, s.description)),
            progress.completed_count(),
            progress.steps.len(),
        ),
        None => (None, 0, 0),
    };
    let lifecycle = lifecycle_line(&state);

    let default_provider = config.provider.default.clone();
    let provider_credential_present = config.resolve_credential(&default_provider).is_some();

    let gate = config
        .harness
        .resolved_checks()
        .iter()
        .map(|check| format!("{} ({})", check.name, cadence_label(check.cadence)))
        .collect();

    Ok(StatusReport {
        branch: git_line(root, &["rev-parse", "--abbrev-ref", "HEAD"]),
        next_step,
        completed,
        total,
        dirty: git_line(root, &["status", "--porcelain"]).is_some_and(|s| !s.trim().is_empty()),
        test_command: config.harness.test_command.clone(),
        default_provider,
        provider_credential_present,
        gate,
        lifecycle,
    })
}

/// Delete the paused-run record only if it is still the one this run was
/// continuing.
///
/// A resumed run can hit the quota again and persist a *new* pause. Deleting
/// unconditionally would erase that fresh record — throwing away the recovery
/// point for work that just happened — so the stored bytes are compared first.
/// Returns whether the record was consumed.
///
/// # Errors
/// Returns an error only if the store cannot be read or written.
fn consume_pause_if_unchanged(store: &Store, original: &[u8]) -> anyhow::Result<bool> {
    if store.get_cache(QUOTA_PAUSE_KEY)?.as_deref() == Some(original) {
        store.delete_cache(QUOTA_PAUSE_KEY)?;
        return Ok(true);
    }
    Ok(false)
}

/// The persisted interruption record, if this project has one.
///
/// A quota-paused run is the only kind that exists today. The record is parsed
/// rather than merely detected: `wait-resume` refuses a record it cannot read, so
/// reporting unreadable bytes as a recoverable pause would promise a recovery
/// that does not exist. A store that cannot be read, or holds something that is
/// not a `PausedRun`, yields `None` — status is a read-only report and must not
/// fail because a cache entry is corrupt.
fn recorded_interruption(root: &Path) -> Option<localpilot_harness::InterruptedRun> {
    let bytes = Store::open(root)
        .get_cache(QUOTA_PAUSE_KEY)
        .ok()
        .flatten()?;
    serde_json::from_slice::<PausedRun>(&bytes)
        .ok()
        .map(|_| localpilot_harness::InterruptedRun::QuotaPause)
}

/// Describe a workspace state in one line, naming the error when there is one.
///
/// Every arm is explicit: a wildcard here is how a new state silently starts
/// rendering as something it is not.
fn lifecycle_line(state: &localpilot_harness::WorkspaceState) -> String {
    use localpilot_harness::{DocumentState, InterruptedRun, OperationState};

    let documents = match &state.documents {
        DocumentState::NoBrief => "no brief.md".to_string(),
        DocumentState::BriefUnreadable(error) => format!("brief.md unreadable: {error}"),
        DocumentState::BriefMalformed(error) => format!("brief.md malformed: {error}"),
        DocumentState::BriefOnly { .. } => "brief only (no plan yet)".to_string(),
        DocumentState::PlanUnreadable { error, .. } => format!("PROGRESS.md unreadable: {error}"),
        DocumentState::PlanMalformed { error, .. } => format!("PROGRESS.md malformed: {error}"),
        DocumentState::PlanUnbound { .. } => {
            "plan not bound to a brief revision (run `localpilot harness adopt` to bind it)"
                .to_string()
        }
        DocumentState::PlanStale {
            recorded, current, ..
        } => format!("plan is stale: built against {recorded}, brief.md is now {current}"),
        DocumentState::PlanBindingUnsupported { recorded, .. } => format!(
            "plan records a brief binding this build does not understand ({recorded}); it was \
             written by a different version of LocalPilot"
        ),
        DocumentState::PlanReady { .. } => "plan current".to_string(),
        DocumentState::PlanComplete { .. } => "plan complete".to_string(),
    };
    match state.operation {
        OperationState::Idle => documents,
        OperationState::Active => format!("{documents}; a harness operation is running"),
        OperationState::Interrupted(InterruptedRun::QuotaPause) => {
            format!("{documents}; a quota-paused run is recorded (`harness wait-resume`)")
        }
    }
}

/// Render why a project cannot run its next step, in the caller's terms.
fn blocked_reason(reason: &localpilot_harness::NotResumable) -> String {
    use localpilot_harness::NotResumable as R;
    match reason {
        R::NoBrief => "brief.md not found; run `localpilot harness intake` first".to_string(),
        R::BriefBroken { detail } => format!("brief.md cannot be used: {detail}"),
        R::NoPlan => "PROGRESS.md not found; run `localpilot harness plan` first".to_string(),
        R::PlanBroken { detail } => format!("PROGRESS.md cannot be used: {detail}"),
        R::Unbound => "PROGRESS.md records no brief revision, so whether it still matches \
             brief.md is unknown. Run `localpilot harness adopt` to declare that this plan \
             belongs to the current brief, or `localpilot harness plan` to replan."
            .to_string(),
        R::Stale { recorded, current } => format!(
            "PROGRESS.md was built against brief revision {recorded}, but brief.md is now \
             {current}. Replan before resuming; the completed steps and their commits are kept."
        ),
        R::BindingUnsupported { recorded } => format!(
            "PROGRESS.md records a brief binding this build does not understand ({recorded}). It \
             was written by a different version of LocalPilot; update LocalPilot or replan. This \
             is not a claim that brief.md changed."
        ),
        R::Complete => "every step in PROGRESS.md is already done".to_string(),
        R::OperationActive => "a harness operation is already running".to_string(),
    }
}

/// Bind an existing unbound plan to the current brief.
///
/// # Errors
/// Returns an error when the project is not in the unbound state, or the plan
/// cannot be written back.
pub fn adopt(root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let revision = localpilot_harness::adopt_plan(root)?;
    writeln!(
        out,
        "PROGRESS.md is now bound to brief revision {revision}. Steps, commits, and attempt \
         counts were left unchanged."
    )?;
    Ok(())
}

/// The plan to run, or a human-readable refusal.
///
/// Every execution entry point goes through this, so "a stale or unbound plan
/// cannot run" is enforced once rather than re-checked by each caller.
fn require_resumable(root: &Path) -> anyhow::Result<()> {
    let state = localpilot_harness::inspect(localpilot_harness::WorkspaceInputs::at(root));
    match localpilot_harness::resumable(&state) {
        Ok(_) => Ok(()),
        Err(reason) => anyhow::bail!(blocked_reason(&reason)),
    }
}

fn cadence_label(cadence: localpilot_config::Cadence) -> &'static str {
    match cadence {
        localpilot_config::Cadence::Step => "step",
        localpilot_config::Cadence::Phase => "phase",
    }
}

pub(crate) fn git_line(root: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

/// Print the harness status to `out`.
///
/// # Errors
/// Returns an error from gathering status or writing output.
pub fn status(root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let report = gather_status(root)?;
    out.write_all(report.render().as_bytes())?;
    Ok(())
}

fn provider_for(
    root: &Path,
    provider_id: Option<&str>,
) -> anyhow::Result<std::sync::Arc<dyn localpilot_llm::ModelProvider>> {
    let config = localpilot_config::load(&ConfigPaths::standard(root), &CliOverrides::default())?;
    let registry = ProviderRegistry::from_config(&config)?;
    match provider_id {
        Some(id) => registry.get(id),
        None => registry.default_provider(),
    }
    .cloned()
    .ok_or_else(|| anyhow::anyhow!("no provider is configured"))
}

/// How one clarifying question is put to the user.
///
/// Guidance questions are open questions with no fixed options, so the two
/// implementations differ only in how the answer is collected: a line of stdin,
/// or the shared choice widget on a terminal. Both keep the same contract — an
/// empty answer delegates that axis to the model's judgment.
pub trait QuestionAsker {
    /// Ask `question` and return the user's answer, empty for "you decide".
    ///
    /// # Errors
    /// Returns an error only when the input surface itself fails.
    fn ask(&mut self, question: &str, out: &mut dyn Write) -> anyhow::Result<String>;
}

/// The stdin asker: today's `write!` + `read_line` loop behind the trait. Every
/// piped/non-TTY intake path keeps its exact behaviour.
pub struct StdinAsker<R: std::io::BufRead>(pub R);

impl<R: std::io::BufRead> QuestionAsker for StdinAsker<R> {
    fn ask(&mut self, question: &str, out: &mut dyn Write) -> anyhow::Result<String> {
        write!(
            out,
            "
{question}
> "
        )?;
        out.flush()?;
        let mut answer = String::new();
        self.0.read_line(&mut answer)?;
        Ok(answer.trim().to_string())
    }
}

/// How a below-threshold guidance assessment resolves the open decisions.
pub enum Clarification<'a> {
    /// Interactive surface: ask each open question and collect one answer per
    /// axis; an empty answer delegates that axis to the model's judgment.
    Ask(&'a mut dyn QuestionAsker),
    /// Non-interactive surface: emit a structured JSON report naming the open
    /// axes on `out` and stop without writing a brief.
    Emit,
    /// Proceed to the brief anyway, recording that the open decisions were
    /// delegated to the model's judgment.
    AssumeJudgment,
}

/// Guidance-gate parameters for one intake run. `None` in [`intake_flow`]
/// means the gate is off and intake behaves exactly as before.
pub struct GuidanceGate<'a> {
    /// Minimum score that proceeds straight to a brief (clamped to 0..=1).
    pub threshold: f32,
    /// Cap on questions asked/reported (floored at 1).
    pub max_questions: usize,
    /// What to do below the threshold.
    pub clarification: Clarification<'a>,
}

/// The result of a gated intake run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntakeOutcome {
    /// `brief.md` was written.
    BriefWritten,
    /// The guidance gate paused the run: open questions were reported and no
    /// brief was written.
    NeedsGuidance,
}

/// Run intake: an idea becomes `brief.md`, with an `.localpilot/intake.jsonl`
/// record.
///
/// # Errors
/// Returns an error if the provider fails or files cannot be written.
pub async fn intake(
    root: &Path,
    model: &str,
    provider_id: Option<&str>,
    idea: &str,
    guidance_override: Option<bool>,
    assume_judgment: bool,
    out: &mut dyn Write,
) -> anyhow::Result<IntakeOutcome> {
    let config = localpilot_config::load(&ConfigPaths::standard(root), &CliOverrides::default())?;
    let provider = provider_for(root, provider_id)?;
    let enabled = guidance_override.unwrap_or(config.harness.guidance.enabled);
    if !enabled {
        return intake_flow(root, provider.as_ref(), model, idea, None, out).await;
    }
    // Interactive Q&A needs a human on both ends; anything else gets the
    // structured JSON report (the non-TTY convention) unless the caller
    // explicitly delegated judgment.
    let interactive = {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
    };
    let mut stdin_asker;
    let clarification = if assume_judgment {
        Clarification::AssumeJudgment
    } else if interactive {
        stdin_asker = StdinAsker(std::io::BufReader::new(std::io::stdin()));
        Clarification::Ask(&mut stdin_asker)
    } else {
        Clarification::Emit
    };
    let gate = GuidanceGate {
        threshold: config.harness.guidance.threshold,
        max_questions: config.harness.guidance.max_questions,
        clarification,
    };
    intake_flow(root, provider.as_ref(), model, idea, Some(gate), out).await
}

/// The intake flow with an injectable provider and clarification source, so
/// every leg is testable offline. With `gate: None` the behaviour (and the
/// `intake.jsonl` record shape) is identical to pre-gate intake.
pub(crate) async fn intake_flow(
    root: &Path,
    provider: &dyn ModelProvider,
    model: &str,
    idea: &str,
    gate: Option<GuidanceGate<'_>>,
    out: &mut dyn Write,
) -> anyhow::Result<IntakeOutcome> {
    let Some(gate) = gate else {
        let brief = run_intake(provider, model, idea).await?;
        std::fs::write(root.join("brief.md"), brief.render())?;
        append_intake_record(
            root,
            serde_json::json!({ "idea": idea, "name": brief.name }),
        )?;
        return Ok(IntakeOutcome::BriefWritten);
    };

    let threshold = gate.threshold.clamp(0.0, 1.0);
    let assessment = localpilot_harness::assess_guidance(provider, model, idea).await?;
    let mut guidance = serde_json::json!({
        "score": assessment.score,
        "threshold": threshold,
        "axes": assessment.axes,
    });

    if assessment.score >= threshold {
        let brief = run_intake(provider, model, idea).await?;
        std::fs::write(root.join("brief.md"), brief.render())?;
        append_intake_record(
            root,
            serde_json::json!({ "idea": idea, "name": brief.name, "guidance": guidance }),
        )?;
        return Ok(IntakeOutcome::BriefWritten);
    }

    let open: Vec<_> = assessment
        .open_axes()
        .into_iter()
        .take(gate.max_questions.max(1))
        .cloned()
        .collect();
    let questions: Vec<String> = open.iter().map(question_for).collect();
    guidance["questions"] = serde_json::json!(questions);

    match gate.clarification {
        Clarification::Emit => {
            let report = serde_json::json!({
                "status": "needs_guidance",
                "score": assessment.score,
                "threshold": threshold,
                "open": open
                    .iter()
                    .zip(&questions)
                    .map(|(axis, question)| {
                        serde_json::json!({ "axis": axis.axis, "question": question })
                    })
                    .collect::<Vec<_>>(),
                "axes": assessment.axes,
                "hint": "answer the questions by re-running intake on a terminal, fold the \
                         decisions into --idea, or re-run with --assume-judgment to let the \
                         model decide",
            });
            writeln!(out, "{}", serde_json::to_string_pretty(&report)?)?;
            append_intake_record(
                root,
                serde_json::json!({ "idea": idea, "guidance": guidance }),
            )?;
            Ok(IntakeOutcome::NeedsGuidance)
        }
        Clarification::AssumeJudgment => {
            guidance["assumed_judgment"] = serde_json::json!(true);
            let brief = run_intake(provider, model, idea).await?;
            std::fs::write(root.join("brief.md"), brief.render())?;
            append_intake_record(
                root,
                serde_json::json!({ "idea": idea, "name": brief.name, "guidance": guidance }),
            )?;
            Ok(IntakeOutcome::BriefWritten)
        }
        Clarification::Ask(asker) => {
            writeln!(
                out,
                "guidance score {:.2} is below the threshold {threshold:.2}: {} decision(s) \
                 the idea does not settle. Answer each question, or press Enter to let the \
                 model use its judgment for that one.",
                assessment.score,
                open.len(),
            )?;
            let mut answers: Vec<(String, String)> = Vec::new();
            for (axis, question) in open.iter().zip(&questions) {
                let answer = asker.ask(question, out)?;
                if !answer.is_empty() {
                    answers.push((axis.axis.clone(), answer));
                }
            }
            guidance["answers"] = serde_json::json!(answers
                .iter()
                .map(|(axis, answer)| serde_json::json!({ "axis": axis, "answer": answer }))
                .collect::<Vec<_>>());

            let (brief_idea, rescored);
            if answers.is_empty() {
                // Every question was delegated — same contract as
                // --assume-judgment, recorded the same way.
                guidance["assumed_judgment"] = serde_json::json!(true);
                brief_idea = idea.to_string();
                rescored = None;
            } else {
                let decisions = answers
                    .iter()
                    .map(|(axis, answer)| format!("- {axis}: {answer}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                brief_idea = format!("{idea}\n\nDecisions provided by the user:\n{decisions}");
                // One bounded re-assessment records whether the answers
                // settled the open axes; it informs the record, it does not
                // re-gate (no loop).
                let second =
                    localpilot_harness::assess_guidance(provider, model, &brief_idea).await?;
                rescored = Some(second.score);
            }
            if let Some(score) = rescored {
                guidance["rescore"] = serde_json::json!(score);
            }
            let brief = run_intake(provider, model, &brief_idea).await?;
            std::fs::write(root.join("brief.md"), brief.render())?;
            append_intake_record(
                root,
                serde_json::json!({ "idea": idea, "name": brief.name, "guidance": guidance }),
            )?;
            Ok(IntakeOutcome::BriefWritten)
        }
    }
}

/// The question asked for an open axis: the model's own settling question
/// when it provided one, otherwise a generic prompt naming the axis.
fn question_for(axis: &localpilot_harness::DecisionAxis) -> String {
    if axis.question.trim().is_empty() {
        format!("What should be decided about: {}?", axis.axis)
    } else {
        axis.question.clone()
    }
}

/// Append one record to the `.localpilot/intake.jsonl` provenance log.
fn append_intake_record(root: &Path, record: serde_json::Value) -> anyhow::Result<()> {
    let intake_dir = root.join(".localpilot");
    std::fs::create_dir_all(&intake_dir)?;
    let mut line = serde_json::to_string(&record)?;
    line.push('\n');
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(intake_dir.join("intake.jsonl"))?;
    file.write_all(line.as_bytes())?;
    Ok(())
}

/// Run planning: `brief.md` becomes `PROGRESS.md`.
///
/// # Errors
/// Returns an error if the brief is missing/invalid or the provider fails.
pub async fn plan(root: &Path, model: &str, provider_id: Option<&str>) -> anyhow::Result<()> {
    let brief_text = std::fs::read_to_string(root.join("brief.md")).map_err(|_| {
        anyhow::anyhow!("brief.md not found; run `localpilot harness intake` first")
    })?;
    let brief = Brief::parse(&brief_text)?;
    let provider = provider_for(root, provider_id)?;
    let summary = repo_summary(root);
    let mut progress = run_plan(provider.as_ref(), model, &brief, &summary).await?;
    // A plan is bound to the brief it was generated from at the moment it is
    // written. Without this every new plan would start life unbound, and the
    // binding would only ever describe plans someone adopted by hand.
    progress.bind_to_brief(localpilot_harness::BriefRevision::of(&brief).as_str());
    std::fs::write(root.join("PROGRESS.md"), progress.render())?;
    Ok(())
}

/// Add a feature to an existing brief and plan, without renumbering completed
/// steps. This is deterministic and needs no provider.
///
/// # Errors
/// Returns an error if the brief or progress files are missing or invalid.
pub fn feature(root: &Path, description: &str) -> anyhow::Result<()> {
    // One inspection, and the documents that get written are the ones it parsed
    // — not a second read that could have moved underneath this command.
    let state = localpilot_harness::inspect(localpilot_harness::WorkspaceInputs::at(root));
    let (mut brief, mut progress) = match state.documents {
        localpilot_harness::DocumentState::PlanReady { brief, progress }
        | localpilot_harness::DocumentState::PlanComplete { brief, progress } => (brief, progress),
        // Anything else is refused with its own named reason: appending to a
        // plan whose relationship to the brief is unknown or superseded would
        // turn that uncertainty into an apparently current plan.
        ref documents => {
            let reason = localpilot_harness::resumable(&state).err().map_or_else(
                || format!("PROGRESS.md is not in a state this command can extend: {documents:?}"),
                |reason| blocked_reason(&reason),
            );
            anyhow::bail!(reason);
        }
    };

    brief.add_requirement(description);
    progress.append_step(format!("Implement: {description}"));
    // The brief just changed, so the plan's binding moves with it in the same
    // command; otherwise this would leave behind exactly the silently-stale plan
    // the binding exists to prevent.
    progress.bind_to_brief(localpilot_harness::BriefRevision::of(&brief).as_str());

    // Two files, so this is not atomic, and the ordering is chosen for what a
    // half-completed run leaves behind rather than for convenience. The plan is
    // written first: if the brief write then fails, the plan records a revision
    // no brief matches, which reads as *stale* and refuses to run. Writing the
    // brief first would leave the opposite window — a changed brief with a plan
    // still bound to the old revision — which is also stale, but only by
    // accident of the same check. Either way the failure is closed, never a plan
    // that claims to be current; the plan-first order makes that the direct
    // consequence of the write that succeeded.
    std::fs::write(root.join("PROGRESS.md"), progress.render())?;
    std::fs::write(root.join("brief.md"), brief.render())?;
    Ok(())
}

/// Preview the discovered quality gate without writing anything. Read-only:
/// discovery proposes, it never runs or ratifies a check.
///
/// # Errors
/// Returns an error only if output cannot be written.
pub fn gate_propose(root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let proposed = propose_gate(root);
    out.write_all(summarize_proposal(&proposed).as_bytes())?;
    Ok(())
}

/// Ratify the discovered gate: write the proposed checks into `.localpilot.toml`
/// as `[[harness.checks]]`, adding only checks not already ratified and leaving
/// the rest of the config untouched. This is the trust boundary — a check does
/// not run until it is ratified here (ADR-0009).
///
/// # Errors
/// Returns an error if `.localpilot.toml` is missing or cannot be written.
pub fn gate_ratify(root: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let config_path = root.join(".localpilot.toml");
    if !config_path.exists() {
        anyhow::bail!(".localpilot.toml not found; run `localpilot init` first");
    }
    let proposed = propose_gate(root);
    if proposed.is_empty() {
        write!(out, "{}", summarize_proposal(&proposed))?;
        return Ok(());
    }
    let existing = std::fs::read_to_string(&config_path)?;
    let config = localpilot_config::load(&ConfigPaths::standard(root), &CliOverrides::default())
        .unwrap_or_else(|_| Config::default());
    let ratified: Vec<String> = config
        .harness
        .checks
        .iter()
        .map(|check| check.name.clone())
        .collect();
    let result = ratify_gate(&existing, &ratified, &proposed);
    if result.added.is_empty() {
        writeln!(
            out,
            "no new checks to ratify ({} already present)",
            result.already_present.len()
        )?;
        return Ok(());
    }
    std::fs::write(&config_path, &result.config_text)?;
    writeln!(
        out,
        "ratified {} check(s) into .localpilot.toml:",
        result.added.len()
    )?;
    // Echo just the newly written checks, with their risk class and warnings.
    let added: Vec<_> = proposed
        .into_iter()
        .filter(|proposal| {
            result
                .added
                .iter()
                .any(|check| check.name == proposal.check.name)
        })
        .collect();
    out.write_all(summarize_proposal(&added).as_bytes())?;
    Ok(())
}

/// Run harness steps from `PROGRESS.md` until none remain, a step is blocked, or
/// the step cap is reached. Each step runs with fresh context.
///
/// # Errors
/// Returns an error if config/provider setup or a step fails.
pub async fn resume(
    root: &Path,
    model: &str,
    provider_id: Option<&str>,
    profile: Profile,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let (events, _rx) = broadcast::channel::<RuntimeEvent>(1024);
    let cancel = CancellationToken::new();
    resume_with_events(
        root,
        model,
        provider_id,
        ResumeRun {
            profile,
            interactivity: Interactivity::NonInteractive,
            trusted: true,
            approver: || Box::new(ScriptedApprover::new(Vec::new())),
        },
        &events,
        &cancel,
        out,
    )
    .await
    .map(|_| ())
}

/// What a resume run actually did.
///
/// A run returns `Ok` for several endings that are not progress: a step blocked
/// by the quality gate or a session-start rule, a cancelled step, a plan that
/// was already finished. `wait-resume` must tell those apart from a step that
/// committed, because the paused-run record it holds is the only way back to the
/// interrupted work — and deleting it after a run that advanced nothing loses
/// that for good.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResumeProgress {
    /// How many steps committed during this run.
    pub committed_steps: usize,
}

impl ResumeProgress {
    /// Whether the run advanced the plan at all.
    #[must_use]
    pub fn advanced(&self) -> bool {
        self.committed_steps > 0
    }
}

/// Runtime settings for a harness resume run.
pub struct ResumeRun<A>
where
    A: FnMut() -> Box<dyn Approver>,
{
    pub profile: Profile,
    pub interactivity: Interactivity,
    pub trusted: bool,
    pub approver: A,
}

/// Run harness steps from `PROGRESS.md` while streaming runtime events to
/// `events`. The CLI uses this with a silent event channel; the TUI subscribes to
/// the same stream and renders model, tool, quota, and approval progress live.
///
/// # Errors
/// Returns an error if config/provider setup or a step fails.
pub async fn resume_with_events<A>(
    root: &Path,
    model: &str,
    provider_id: Option<&str>,
    mut run: ResumeRun<A>,
    events: &broadcast::Sender<RuntimeEvent>,
    cancel: &CancellationToken,
    out: &mut dyn Write,
) -> anyhow::Result<ResumeProgress>
where
    A: FnMut() -> Box<dyn Approver>,
{
    // The lifecycle gate runs before the provider is resolved: a stale or
    // unbound plan is refused without a network call or a session being opened.
    require_resumable(root)?;
    let config = localpilot_config::load(&ConfigPaths::standard(root), &CliOverrides::default())
        .unwrap_or_else(|_| Config::default());
    let provider = provider_for(root, provider_id)?;
    let workspace = crate::session_cmd::workspace_with_read_roots(root, &config)?;
    let rules = RuleEngine::with_baseline(&config.harness.rules);
    let test_command = config.harness.test_command.clone();
    let checks = config.harness.checks.clone();
    let max_attempts = config.harness.attempts_per_step;
    // Ratifying the gate grants its tool identity a relaxed-profile allowance, so
    // a non-interactive run can execute the (project-write) checks the user
    // committed without prompting (ADR-0009). The allowance is scoped to the gate
    // identity, which only ever runs ratified checks — never arbitrary shell.
    let gate_allowance = if checks.is_empty() && test_command.is_none() {
        Vec::new()
    } else {
        vec![QUALITY_CHECK_TOOL.to_string()]
    };
    // Connect MCP servers once; each step builds a fresh registry over them.
    let mcp = crate::mcp::McpTools::load(&config).await;

    let mut progress_made = ResumeProgress::default();
    const MAX_STEPS: usize = 100;
    for _ in 0..MAX_STEPS {
        // Re-inspect each iteration: the step that just ran edits PROGRESS.md,
        // so the loop's view has to come from disk again. Going through the
        // shared gate rather than a local `.ok()` chain also means a step that
        // corrupts the plan stops the run and says so, instead of parsing to
        // `None` and being announced as "all steps complete".
        let state = localpilot_harness::inspect(localpilot_harness::WorkspaceInputs::at(root));
        let next_step = match localpilot_harness::resumable(&state) {
            Ok(progress) => progress
                .next_incomplete()
                .map(|step| step.description.clone()),
            Err(localpilot_harness::NotResumable::Complete) => None,
            Err(reason) => anyhow::bail!(blocked_reason(&reason)),
        };
        if next_step.is_none() {
            writeln!(out, "all steps complete")?;
            // Advisory completion retrospective (best-effort): review the finished
            // work against the brief and record any lessons to LESSONS.md. A
            // provider error here never breaks a finished run, and the review never
            // blocks completion or edits code.
            if let Ok(Some(retro)) =
                localpilot_harness::run_and_record(&*provider, model, root).await
            {
                writeln!(out, "{}", retro.render_summary())?;
                // Also offer each lesson to LocalMind's review-gated queue, so a
                // human can promote it to memory instead of it living only in the
                // human-editable LESSONS.md mirror. Advisory and non-blocking: a
                // failed enqueue never breaks a finished run (the lesson is still in
                // LESSONS.md), and a candidate reaches memory only after human review.
                let mut offered = 0usize;
                for lesson in &retro.lessons {
                    if let Ok(Some(_)) = localpilot_localmind::write_retrospective_lesson(
                        root,
                        &localpilot_localmind::RetrospectiveLesson::new(lesson.as_str()),
                    ) {
                        offered += 1;
                    }
                }
                if offered > 0 {
                    writeln!(out, "  {offered} lesson(s) offered to LocalMind review")?;
                }
            }
            // Advisory whole-repo teardown sweep (best-effort): when opted in, run
            // the read-only cleanup-audit pass alongside the retrospective and print
            // its ranked findings. It is deterministic and offline — no provider
            // call — and read-only by construction, so it never blocks completion,
            // edits code, or commits. The result is ignored so even a write hiccup
            // on `out` cannot break a finished run.
            let _ = crate::self_review_cmd::run_completion_sweep(
                root,
                config.harness.teardown_sweep,
                out,
            );
            break;
        }

        // Apply the built-in safety rails so a harness step in a project with no
        // `[harness]` budget/timeout still self-bounds (ADR-0055); harness steps run
        // headless, so they use the headless profile. Explicit config wins.
        let rails = config
            .harness
            .resolved_rails(matches!(run.interactivity, Interactivity::Interactive));
        let mut runtime = build_runtime(
            root,
            Arc::clone(&provider),
            workspace.clone(),
            run.profile,
            run.interactivity,
            run.trusted,
            model,
            &mcp,
            localpilot_harness::effective_context_limit(
                provider.declaration().max_context_tokens,
                config.harness.context_token_limit,
                provider.declaration().max_output_tokens,
            ),
            rails.tool_call_budget,
            rails.tool_call_budget_max,
            rails.budget_explicit,
            rails.turn_timeout_secs,
            compaction_mode(config.compaction.mode),
            localpilot_harness::SummarizerTuning::from_config(&config.compaction),
            gate_allowance.clone(),
            config.harness.rules.clone(),
            config.harness.claim_gate.is_enabled(),
            &config.tools,
            (run.approver)(),
        );
        localpilot_harness::register_project_analysis_context(
            root,
            config.context.project_analysis,
            config.docs.lookup_policy,
            &mut runtime,
        );
        localpilot_harness::register_project_instructions_context(
            root,
            config.context.inject_instructions,
            config.context.instruction_char_budget,
            &mut runtime,
        );
        localpilot_localmind::register_context_hook(root, &mut runtime);
        let outcome = resume_one_step_with_events(
            &mut runtime,
            root,
            &rules,
            test_command.as_deref(),
            &checks,
            max_attempts,
            events,
            cancel,
        )
        .await?;
        // Learn from the finished step. Each harness step is its own session, so
        // close it out into LocalMind here (best-effort; skips an empty session)
        // — this is how autonomous runs produce reviewed memory, not just the
        // interactive REPL.
        crate::context_inject::close_out(root, runtime.session_id());
        let gate = render_gate(&outcome.gate);
        if !gate.is_empty() {
            write!(out, "{gate}")?;
        }
        if outcome.committed {
            progress_made.committed_steps += 1;
            writeln!(out, "step {} complete", outcome.step_number)?;
            // A committed step can still carry a reason when the phase-cadence
            // gate ran (plan boundary) and blocked — e.g. a failing dependency
            // audit. Surface it and stop rather than reporting a clean run.
            if let Some(reason) = &outcome.blocked_reason {
                writeln!(out, "phase gate blocked: {reason}")?;
                break;
            }
        } else {
            writeln!(
                out,
                "step {} blocked: {}",
                outcome.step_number,
                outcome.blocked_reason.as_deref().unwrap_or("unknown")
            )?;
            break;
        }
    }
    Ok(progress_made)
}

/// Render the quality-gate outcomes for a step as a bounded, one-line-per-check
/// summary (which checks ran, pass/fail, what was auto-fixed). The per-check
/// `detail` is already bounded and redacted; it is omitted here to keep the run
/// log readable — `harness status` and the transcript carry the detail.
fn render_gate(outcomes: &[CheckOutcome]) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for outcome in outcomes {
        let status = match outcome.status {
            CheckStatus::Passed => "passed",
            CheckStatus::Failed => "failed",
            CheckStatus::Denied => "denied",
            CheckStatus::Errored => "errored",
        };
        let fixed = if outcome.fixed { " (auto-fixed)" } else { "" };
        let _ = writeln!(s, "  check {}: {status}{fixed}", outcome.name);
    }
    s
}

/// Continue a run that paused on a provider quota/rate limit, if it is now safe.
///
/// # Errors
/// Returns an error if the paused-run file is unreadable or resume fails.
pub async fn wait_resume(
    root: &Path,
    model: &str,
    provider_id: Option<&str>,
    profile: Profile,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let (events, _rx) = broadcast::channel::<RuntimeEvent>(1024);
    let cancel = CancellationToken::new();
    wait_resume_with_events(
        root,
        model,
        provider_id,
        ResumeRun {
            profile,
            interactivity: Interactivity::NonInteractive,
            trusted: true,
            approver: || Box::new(ScriptedApprover::new(Vec::new())),
        },
        &events,
        &cancel,
        out,
    )
    .await
    .map(|_| ())
}

/// Continue a quota-paused run through the streaming resume path, if allowed by
/// policy.
///
/// # Errors
/// Returns an error if the paused-run file is unreadable or resume fails.
pub async fn wait_resume_with_events<A>(
    root: &Path,
    model: &str,
    provider_id: Option<&str>,
    run: ResumeRun<A>,
    events: &broadcast::Sender<RuntimeEvent>,
    cancel: &CancellationToken,
    out: &mut dyn Write,
) -> anyhow::Result<ResumeProgress>
where
    A: FnMut() -> Box<dyn Approver>,
{
    let store = Store::open(root);
    let Some(bytes) = store.get_cache(QUOTA_PAUSE_KEY)? else {
        writeln!(out, "no paused run")?;
        return Ok(ResumeProgress::default());
    };
    let paused: PausedRun = serde_json::from_slice(&bytes)
        .map_err(|e| anyhow::anyhow!("invalid paused-run file: {e}"))?;
    // A pause record does not make a superseded plan runnable: waiting out a
    // quota window and then executing against a stale plan is the same mistake,
    // just later.
    require_resumable(root)?;

    let config = localpilot_config::load(&ConfigPaths::standard(root), &CliOverrides::default())
        .unwrap_or_else(|_| Config::default());
    let policy = ResumePolicy::from(&config.quota);

    // Wait until the pause window elapses (re-checking safety gates and
    // cancellation each poll), then resume — instead of printing an ETA and
    // exiting. Bounded by `max_wait` (once `waited` passes it `decide_resume`
    // returns `BlockedBy`, ending the loop) and cancellable at any sleep.
    loop {
        if cancel.is_cancelled() {
            writeln!(out, "wait cancelled")?;
            return Ok(ResumeProgress::default());
        }
        let now = now_unix();
        let ctx = ResumeContext {
            window_elapsed: paused.resume_eligible_unix.is_none_or(|t| now >= t),
            // The between-steps CLI resume is always at a step boundary and has no
            // pending interactive approval; the interactive path feeds real values.
            at_step_boundary: true,
            workspace_clean: !workspace_dirty(root),
            pending_destructive_approval: false,
            user_cancelled: cancel.is_cancelled(),
            // Only an explicit `--provider` that differs from the paused run's
            // provider counts as an identity change; resuming with no override
            // stays on the same provider.
            provider_identity_changed: provider_id.is_some_and(|p| p != paused.provider_id),
            waited: Duration::from_secs(now.saturating_sub(paused.paused_at_unix)),
        };

        match decide_resume(&policy, &ctx) {
            ResumeDecision::Resume => {
                // Re-check immediately before continuing: the wait can last
                // hours and brief.md can change inside it.
                require_resumable(root)?;
                writeln!(out, "resuming paused run at step {}", paused.step_number)?;
                // The record is consumed AFTER a successful continuation, never
                // before it. Everything between here and a finished step can
                // still fail — resolving the provider, building the workspace,
                // connecting MCP servers, the step itself — and the record is
                // the only way back to the interrupted work.
                let outcome =
                    resume_with_events(root, model, provider_id, run, events, cancel, out).await;
                // `Ok` alone is not progress. A step blocked by a rule or the
                // quality gate, a cancelled step, and a plan that was already
                // finished all return `Ok` — and none of them advanced the work
                // the record was holding. Consume it only when a step actually
                // committed; a fresh pause written by this run has different
                // bytes and is left alone by the comparison.
                if outcome.as_ref().is_ok_and(ResumeProgress::advanced) {
                    consume_pause_if_unchanged(&store, &bytes)?;
                }
                return outcome;
            }
            ResumeDecision::AskUser => {
                writeln!(
                    out,
                    "auto_resume is 'ask'; set quota.auto_resume = run|global to continue automatically"
                )?;
                return Ok(ResumeProgress::default());
            }
            ResumeDecision::BlockedBy(reason) => {
                writeln!(out, "cannot resume: {reason}")?;
                return Ok(ResumeProgress::default());
            }
            ResumeDecision::Wait => {
                let nap = wait_nap(&paused, &policy, now, WAIT_POLL_CAP_SECS)
                    .unwrap_or(Duration::from_secs(1));
                let eta = paused
                    .resume_eligible_unix
                    .map_or(0, |t| t.saturating_sub(now));
                writeln!(
                    out,
                    "paused ({}); waiting ~{}s (resume eligible in ~{eta}s)",
                    paused.reason,
                    nap.as_secs()
                )?;
                tokio::select! {
                    () = cancel.cancelled() => {
                        writeln!(out, "wait cancelled")?;
                        return Ok(ResumeProgress::default());
                    }
                    () = tokio::time::sleep(nap) => {}
                }
            }
        }
    }
}

/// How long a paused run should sleep before re-deciding: `Some(nap)` to sleep
/// then re-check, `None` when the window has elapsed (act now). The nap is
/// clamped to `poll_cap` (so gates and cancellation are re-checked periodically)
/// and to the time left before `max_wait` (so the loop never sleeps past its own
/// deadline — `decide_resume` then returns `BlockedBy` and the loop ends). Pure,
/// so it is unit-tested without a clock.
fn wait_nap(
    paused: &PausedRun,
    policy: &ResumePolicy,
    now: u64,
    poll_cap: u64,
) -> Option<Duration> {
    let eligible = paused.resume_eligible_unix?;
    if now >= eligible {
        return None;
    }
    let waited = now.saturating_sub(paused.paused_at_unix);
    let until_max = policy.max_wait.as_secs().saturating_sub(waited);
    if until_max == 0 {
        return None;
    }
    let nap = (eligible - now).min(poll_cap).min(until_max).max(1);
    Some(Duration::from_secs(nap))
}

/// Re-check cadence while waiting on a quota pause, so cancellation and the
/// safety gates are re-evaluated at least this often even for a long window.
const WAIT_POLL_CAP_SECS: u64 = 30;

fn workspace_dirty(root: &Path) -> bool {
    git_line(root, &["status", "--porcelain"]).is_some_and(|s| !s.trim().is_empty())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[allow(clippy::too_many_arguments)] // a runtime genuinely composes these collaborators
fn build_runtime(
    root: &Path,
    provider: Arc<dyn ModelProvider>,
    workspace: Workspace,
    profile: Profile,
    interactivity: Interactivity,
    trusted: bool,
    model: &str,
    mcp: &crate::mcp::McpTools,
    context_token_limit: usize,
    tool_call_budget: Option<usize>,
    tool_call_budget_max: Option<usize>,
    tool_budget_explicit: bool,
    turn_timeout_secs: Option<u64>,
    compaction_mode: localpilot_harness::CompactionMode,
    summarizer_tuning: localpilot_harness::SummarizerTuning,
    allowlist: Vec<String>,
    rules: IndexMap<String, RuleSeverity>,
    enforce_claim_gate: bool,
    tools: &localpilot_config::ToolsConfig,
    approver: Box<dyn Approver>,
) -> SessionRuntime {
    let mut registry = mcp.registry();
    let broker = crate::mcp::install_broker(tools, &mut registry);
    let mut runtime = SessionRuntime::new(
        provider,
        registry,
        PermissionEngine::new(profile, allowlist),
        approver,
        Store::open(root),
        workspace,
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            model: model.to_string(),
            interactivity,
            trusted,
            context_token_limit,
            tool_call_budget,
            tool_call_budget_max,
            tool_budget_explicit,
            turn_timeout: turn_timeout_secs.map(std::time::Duration::from_secs),
            compaction_mode,
            summarizer_tuning,
            rules,
            enforce_claim_gate,
            tool_marker_enabled: tools.marker,
            enforce_readable_errors: tools.readable_errors,
            repair_mode: tools.repair,
            elide_seen_reads: tools.elide_seen_reads,
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    runtime.set_broker(broker);
    if let Some(agents) = crate::agents_cmd::session_agents(root) {
        runtime.set_agents(agents);
    }
    runtime
}

fn compaction_mode(mode: localpilot_config::CompactionMode) -> localpilot_harness::CompactionMode {
    match mode {
        localpilot_config::CompactionMode::Deterministic => {
            localpilot_harness::CompactionMode::Deterministic
        }
        localpilot_config::CompactionMode::SmartWithFallback => {
            localpilot_harness::CompactionMode::SmartWithFallback
        }
    }
}

fn repo_summary(root: &Path) -> String {
    let mut entries: Vec<String> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    entries.sort();
    format!("Top-level entries: {}", entries.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_llm::FakeProvider;

    fn paused_at(paused: u64, eligible: Option<u64>) -> PausedRun {
        PausedRun {
            paused_at_unix: paused,
            reason: "limit".to_string(),
            resume_eligible_unix: eligible,
            step_number: 1,
            provider_id: "p".to_string(),
            attempt: 1,
        }
    }

    fn wait_policy(max_wait_secs: u64) -> localpilot_quota::ResumePolicy {
        localpilot_quota::ResumePolicy {
            mode: localpilot_quota::ResumeMode::Run,
            max_wait: Duration::from_secs(max_wait_secs),
            requires_clean_workspace: false,
            requires_no_pending_approval: false,
            only_at_step_boundary: false,
        }
    }

    /// A brief and a plan bound to it: the minimum state in which the harness
    /// will agree to run a step at all.
    fn runnable_project(root: &Path) {
        const BRIEF: &str = "# Brief: thing\n\n## Summary\n\nDo the thing.\n\n\
## Requirements\n\n- It works\n\n## Constraints\n\n- Be small\n\n\
## Non-Goals\n\n- World peace\n\n## Acceptance Criteria\n\n- A test passes\n";
        std::fs::write(root.join("brief.md"), BRIEF).unwrap();
        let revision =
            localpilot_harness::BriefRevision::of(&Brief::parse(BRIEF).unwrap()).to_string();
        std::fs::write(
            root.join("PROGRESS.md"),
            format!(
                "# Progress: thing\nBranch: feature/thing\nBrief: {revision}\n\n## Steps\n\n\
- [ ] 1. Implement it\n"
            ),
        )
        .unwrap();
    }

    /// Turn a runnable project stale by changing what the brief asks for.
    fn make_stale(root: &Path) {
        let brief = std::fs::read_to_string(root.join("brief.md")).unwrap();
        std::fs::write(
            root.join("brief.md"),
            brief.replace("- It works", "- It works, and it is fast"),
        )
        .unwrap();
    }

    fn wait_resume_run() -> ResumeRun<impl FnMut() -> Box<dyn Approver>> {
        ResumeRun {
            profile: Profile::Default,
            interactivity: Interactivity::NonInteractive,
            trusted: false,
            approver: || Box::new(ScriptedApprover::new(Vec::new())) as Box<dyn Approver>,
        }
    }

    /// A project the resume path will reach: a runnable plan, a configured
    /// provider so the run gets past provider resolution, and a git repository
    /// so the session-start rules have something to inspect. No network is
    /// touched — the provider is constructed, never called.
    fn resumable_project_with_provider(root: &Path) {
        std::fs::write(
            root.join(".localpilot.toml"),
            "[quota]\nauto_resume = \"run\"\nmax_wait_minutes = 360\n\n\
[provider]\ndefault = \"local\"\n\n\
[providers.local]\nkind = \"openai-compatible\"\n\
base_url = \"http://127.0.0.1:9/v1\"\nmodel = \"m\"\napi_key = \"x\"\n",
        )
        .unwrap();
        runnable_project(root);
        for args in [
            vec!["init"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
            vec!["add", "-A"],
            vec!["commit", "-m", "initial"],
        ] {
            let status = std::process::Command::new("git")
                .args(&args)
                .current_dir(root)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        }
    }

    #[tokio::test]
    async fn a_delegated_run_blocked_at_session_start_keeps_the_paused_run() {
        // The run reaches the executor and returns `Ok` — the step was refused
        // by a session-start rule, not by an error. That is exactly the shape
        // that used to consume the record: a successful call that advanced
        // nothing.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        resumable_project_with_provider(root);
        // An unrelated uncommitted change is what the baseline session-start
        // rule refuses.
        std::fs::write(root.join("scratch.txt"), "unrelated work\n").unwrap();

        let store = Store::open(root);
        let now = now_unix();
        let original = serde_json::to_vec(&paused_at(now, Some(now))).unwrap();
        store.put_cache(QUOTA_PAUSE_KEY, &original).unwrap();

        let (events_tx, _rx) = broadcast::channel::<RuntimeEvent>(16);
        let cancel = CancellationToken::new();
        let mut out: Vec<u8> = Vec::new();
        let progress = wait_resume_with_events(
            root,
            "m",
            None,
            wait_resume_run(),
            &events_tx,
            &cancel,
            &mut out,
        )
        .await
        .expect("a blocked step is not an error");

        assert_eq!(
            progress.committed_steps,
            0,
            "nothing advanced: {}",
            String::from_utf8_lossy(&out)
        );
        assert_eq!(
            store.get_cache(QUOTA_PAUSE_KEY).unwrap().as_deref(),
            Some(original.as_slice()),
            "the paused run is byte-identical after a blocked run"
        );
    }

    #[tokio::test]
    async fn a_cancelled_wait_keeps_the_paused_run() {
        // Cancellation is a stop, not a completion. The record is what the next
        // attempt resumes from.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        resumable_project_with_provider(root);

        let store = Store::open(root);
        let now = now_unix();
        let original = serde_json::to_vec(&paused_at(now, Some(now + 3600))).unwrap();
        store.put_cache(QUOTA_PAUSE_KEY, &original).unwrap();

        let (events_tx, _rx) = broadcast::channel::<RuntimeEvent>(16);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut out: Vec<u8> = Vec::new();
        let progress = wait_resume_with_events(
            root,
            "m",
            None,
            wait_resume_run(),
            &events_tx,
            &cancel,
            &mut out,
        )
        .await
        .unwrap();

        assert_eq!(progress.committed_steps, 0);
        assert_eq!(
            store.get_cache(QUOTA_PAUSE_KEY).unwrap().as_deref(),
            Some(original.as_slice()),
            "a cancelled wait consumes nothing"
        );
    }

    #[tokio::test]
    async fn a_failure_after_the_policy_decision_keeps_the_paused_run() {
        // Everything between the decision to resume and a finished step can
        // fail. Here the project has no provider configured, so the continuation
        // fails while resolving one — after the policy said Resume, which is
        // exactly the window in which the record used to be deleted already.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".localpilot.toml"),
            "[quota]\nauto_resume = \"run\"\nmax_wait_minutes = 360\n",
        )
        .unwrap();
        runnable_project(dir.path());

        let store = Store::open(dir.path());
        let now = now_unix();
        store
            .put_cache(
                QUOTA_PAUSE_KEY,
                &serde_json::to_vec(&paused_at(now, Some(now))).unwrap(),
            )
            .unwrap();

        let (events_tx, _rx) = broadcast::channel::<RuntimeEvent>(16);
        let cancel = CancellationToken::new();
        let mut out: Vec<u8> = Vec::new();
        let result = wait_resume_with_events(
            dir.path(),
            "model",
            None,
            wait_resume_run(),
            &events_tx,
            &cancel,
            &mut out,
        )
        .await;

        assert!(result.is_err(), "no provider is configured");
        assert!(
            store.get_cache(QUOTA_PAUSE_KEY).unwrap().is_some(),
            "the paused run survives a failure after the policy decision"
        );
    }

    #[test]
    fn consuming_a_pause_never_erases_a_newer_one() {
        // A resumed run can hit the quota again and persist a fresh record.
        // Deleting unconditionally would throw away the recovery point for work
        // that just happened.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path());
        let now = now_unix();
        let original = serde_json::to_vec(&paused_at(now, Some(now))).unwrap();
        let newer = serde_json::to_vec(&paused_at(now + 60, Some(now + 120))).unwrap();

        store.put_cache(QUOTA_PAUSE_KEY, &newer).unwrap();
        assert!(
            !consume_pause_if_unchanged(&store, &original).unwrap(),
            "a different record is not this run's to delete"
        );
        assert_eq!(
            store.get_cache(QUOTA_PAUSE_KEY).unwrap().as_deref(),
            Some(newer.as_slice())
        );

        store.put_cache(QUOTA_PAUSE_KEY, &original).unwrap();
        assert!(consume_pause_if_unchanged(&store, &original).unwrap());
        assert!(store.get_cache(QUOTA_PAUSE_KEY).unwrap().is_none());
    }

    #[test]
    fn status_reports_a_readable_pause_and_stays_silent_about_an_unreadable_one() {
        // `wait-resume` refuses a record it cannot parse, so status must not
        // advertise one as recoverable. The two cases are pinned together
        // because the difference between them is the whole point.
        let dir = tempfile::tempdir().unwrap();
        runnable_project(dir.path());
        let store = Store::open(dir.path());
        let now = now_unix();

        store
            .put_cache(
                QUOTA_PAUSE_KEY,
                &serde_json::to_vec(&paused_at(now, Some(now))).unwrap(),
            )
            .unwrap();
        let report = gather_status(dir.path()).unwrap();
        assert!(
            report.lifecycle.contains("quota-paused"),
            "{}",
            report.lifecycle
        );

        store
            .put_cache(QUOTA_PAUSE_KEY, b"not json at all")
            .unwrap();
        let report = gather_status(dir.path()).unwrap();
        assert!(
            !report.lifecycle.contains("quota-paused"),
            "an unreadable record is not a recoverable run: {}",
            report.lifecycle
        );
    }

    #[tokio::test]
    async fn wait_resume_refuses_a_stale_plan_and_keeps_the_paused_run() {
        // The pause record is the only way back to the interrupted work. A
        // refusal must never be the thing that destroys it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".localpilot.toml"),
            "[quota]\nauto_resume = \"run\"\nmax_wait_minutes = 360\n",
        )
        .unwrap();
        runnable_project(dir.path());
        make_stale(dir.path());

        let store = Store::open(dir.path());
        let now = now_unix();
        store
            .put_cache(
                QUOTA_PAUSE_KEY,
                &serde_json::to_vec(&paused_at(now, Some(now))).unwrap(),
            )
            .unwrap();

        let (events_tx, _rx) = broadcast::channel::<RuntimeEvent>(16);
        let cancel = CancellationToken::new();
        let mut out: Vec<u8> = Vec::new();
        let error = wait_resume_with_events(
            dir.path(),
            "model",
            None,
            wait_resume_run(),
            &events_tx,
            &cancel,
            &mut out,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("Replan"), "{error}");
        assert!(
            store.get_cache(QUOTA_PAUSE_KEY).unwrap().is_some(),
            "the paused run survives the refusal"
        );
    }

    #[tokio::test]
    async fn a_brief_edited_during_the_wait_does_not_consume_the_paused_run() {
        // The wait can last hours, and the brief can change inside it. The
        // record is consumed only once every precondition still holds, so a plan
        // that goes stale mid-wait is refused with the pause intact rather than
        // deleted on the way to a refusal.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".localpilot.toml"),
            "[quota]\nauto_resume = \"run\"\nmax_wait_minutes = 360\n",
        )
        .unwrap();
        runnable_project(dir.path());

        let store = Store::open(dir.path());
        let now = now_unix();
        // Eligible two seconds out: the first poll waits, and the edit lands
        // well before the window elapses.
        store
            .put_cache(
                QUOTA_PAUSE_KEY,
                &serde_json::to_vec(&paused_at(now, Some(now + 2))).unwrap(),
            )
            .unwrap();

        let (events_tx, _rx) = broadcast::channel::<RuntimeEvent>(16);
        let cancel = CancellationToken::new();
        let mut out: Vec<u8> = Vec::new();
        let root = dir.path().to_path_buf();
        let waiter = wait_resume_with_events(
            dir.path(),
            "model",
            None,
            wait_resume_run(),
            &events_tx,
            &cancel,
            &mut out,
        );
        let editor = async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            make_stale(&root);
        };
        let (result, ()) = tokio::join!(waiter, editor);

        let error = result.unwrap_err();
        assert!(error.to_string().contains("Replan"), "{error}");
        assert!(
            store.get_cache(QUOTA_PAUSE_KEY).unwrap().is_some(),
            "the paused run survives a mid-wait staleness"
        );
    }

    #[tokio::test]
    async fn wait_resume_cancellation_during_wait_returns_promptly_without_delegating() {
        // Cancellation DURING the wait (not the pre-loop short-circuit): reach the Wait branch,
        // then cancel — with no real sleep. An isolated project config makes `ResumePolicy`
        // allow Wait; a recently-paused, future-eligible record makes `decide_resume` return
        // `Wait` (waited ~= 0 < max_wait, window not elapsed, not user-cancelled). The token is
        // cancelled only AFTER the waiter parks in the wait `select!`, so `/wait-resume` prints
        // the "waiting ..." line then returns promptly with "wait cancelled", never delegates
        // into `resume_with_events`, and leaves the paused record intact. Hermetic: no provider
        // or network path is built, and the nap sleep never elapses.
        let dir = tempfile::tempdir().unwrap();
        // Isolated project config: Wait allowed, no providers/MCP (no egress).
        std::fs::write(
            dir.path().join(".localpilot.toml"),
            "[quota]\nauto_resume = \"run\"\nmax_wait_minutes = 360\n",
        )
        .unwrap();
        // A runnable project: waiting out a quota window is refused up front for
        // a plan that could never run afterwards, so the fixture has to be one
        // that could. This is scenery for the cancellation path under test.
        runnable_project(dir.path());
        let store = Store::open(dir.path());
        let now = now_unix();
        // Recent pause (waited ~= 0), eligible an hour out (window not elapsed -> Wait).
        let paused = paused_at(now, Some(now + 3600));
        store
            .put_cache(QUOTA_PAUSE_KEY, &serde_json::to_vec(&paused).unwrap())
            .unwrap();

        let (events_tx, _events_rx) = broadcast::channel::<RuntimeEvent>(16);
        let cancel = CancellationToken::new();
        let run = ResumeRun {
            profile: Profile::Default,
            interactivity: Interactivity::NonInteractive,
            trusted: false,
            approver: || Box::new(ScriptedApprover::new(Vec::new())) as Box<dyn Approver>,
        };
        let mut out: Vec<u8> = Vec::new();
        let waiter = wait_resume_with_events(
            dir.path(),
            "model",
            None,
            run,
            &events_tx,
            &cancel,
            &mut out,
        );
        // `join!` polls the waiter first (it runs to the wait `select!`, registering the nap
        // timer + cancel waker, then parks Pending); the canceller yields twice so the waiter
        // is parked, then cancels — the `cancel.cancelled()` arm wins on the next poll and the
        // nap sleep never elapses.
        let canceller = async {
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            cancel.cancel();
        };
        let (result, ()) = tokio::join!(waiter, canceller);
        result.expect("wait-resume returns Ok on cancellation");

        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("waiting"),
            "it reached the Wait branch (printed the waiting line): {text}"
        );
        assert!(
            text.contains("wait cancelled"),
            "cancellation during the wait is reported: {text}"
        );
        assert!(
            !text.contains("resuming paused run"),
            "wait-resume must NOT delegate into resume_with_events on cancel: {text}"
        );
        assert!(
            store.get_cache(QUOTA_PAUSE_KEY).unwrap().is_some(),
            "the paused record is left intact (not consumed) on a cancelled wait"
        );
    }

    #[test]
    fn wait_nap_clamps_to_poll_cap_and_max_wait_and_stops_when_elapsed() {
        // Eligible in 100s, poll cap 30 → nap 30 (re-check cadence).
        let p = paused_at(0, Some(100));
        assert_eq!(
            wait_nap(&p, &wait_policy(3600), 0, 30),
            Some(Duration::from_secs(30))
        );
        // Window elapsed → act now.
        assert_eq!(wait_nap(&p, &wait_policy(3600), 100, 30), None);
        // Clamped to the time left before max_wait: waited 50 of max 60 → nap 10.
        assert_eq!(
            wait_nap(&p, &wait_policy(60), 50, 30),
            Some(Duration::from_secs(10))
        );
        // Past max_wait → no nap; decide_resume returns BlockedBy and the loop ends.
        assert_eq!(wait_nap(&p, &wait_policy(60), 60, 30), None);
        // No eligible time recorded → act now.
        assert_eq!(
            wait_nap(&paused_at(0, None), &wait_policy(3600), 0, 30),
            None
        );
    }

    const VALID_BRIEF: &str = "# Brief: widget\n\n## Summary\n\nBuild the widget.\n\n\
## Requirements\n\n- It works\n\n## Constraints\n\n- Small\n\n\
## Non-Goals\n\n- Everything else\n\n## Acceptance Criteria\n\n- A test passes\n";

    const LOW_GUIDANCE: &str = r#"{"axes":[
        {"axis":"scope","resolved":true,"evidence":"the widget","question":""},
        {"axis":"platform","resolved":false,"evidence":"not specified","question":"Which platform must this run on?"},
        {"axis":"persistence","resolved":false,"evidence":"not specified","question":"Where is widget state stored?"}
    ]}"#;

    const HIGH_GUIDANCE: &str = r#"{"axes":[
        {"axis":"scope","resolved":true,"evidence":"the widget","question":""},
        {"axis":"platform","resolved":true,"evidence":"on Windows","question":""}
    ]}"#;

    fn last_record(root: &Path) -> serde_json::Value {
        let log = std::fs::read_to_string(root.join(".localpilot/intake.jsonl")).unwrap();
        serde_json::from_str(log.lines().last().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn intake_with_gate_off_writes_brief_and_plain_record() {
        let dir = tempfile::tempdir().unwrap();
        let provider = FakeProvider::new().text(VALID_BRIEF);
        let mut out = Vec::new();
        let outcome = intake_flow(dir.path(), &provider, "m", "build a widget", None, &mut out)
            .await
            .unwrap();
        assert_eq!(outcome, IntakeOutcome::BriefWritten);
        assert!(dir.path().join("brief.md").exists());
        let record = last_record(dir.path());
        assert_eq!(record["idea"], "build a widget");
        assert_eq!(record["name"], "widget");
        // The pre-gate record shape is unchanged when the gate is off.
        assert!(record.get("guidance").is_none());
        assert!(out.is_empty(), "gate-off intake writes nothing to out");
    }

    #[tokio::test]
    async fn above_threshold_proceeds_and_records_the_assessment() {
        let dir = tempfile::tempdir().unwrap();
        let provider = FakeProvider::new().text(HIGH_GUIDANCE).text(VALID_BRIEF);
        let mut out = Vec::new();
        let gate = GuidanceGate {
            threshold: 0.7,
            max_questions: 5,
            clarification: Clarification::Emit,
        };
        let outcome = intake_flow(
            dir.path(),
            &provider,
            "m",
            "build a widget on Windows",
            Some(gate),
            &mut out,
        )
        .await
        .unwrap();
        assert_eq!(outcome, IntakeOutcome::BriefWritten);
        let record = last_record(dir.path());
        assert!((record["guidance"]["score"].as_f64().unwrap() - 1.0).abs() < 1e-6);
        assert!(record["guidance"].get("questions").is_none());
    }

    #[tokio::test]
    async fn below_threshold_emit_reports_open_axes_and_writes_no_brief() {
        let dir = tempfile::tempdir().unwrap();
        let provider = FakeProvider::new().text(LOW_GUIDANCE);
        let mut out = Vec::new();
        let gate = GuidanceGate {
            threshold: 0.7,
            max_questions: 5,
            clarification: Clarification::Emit,
        };
        let outcome = intake_flow(
            dir.path(),
            &provider,
            "m",
            "build a widget",
            Some(gate),
            &mut out,
        )
        .await
        .unwrap();
        assert_eq!(outcome, IntakeOutcome::NeedsGuidance);
        assert!(!dir.path().join("brief.md").exists());
        let report: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(report["status"], "needs_guidance");
        assert_eq!(report["open"].as_array().unwrap().len(), 2);
        assert_eq!(
            report["open"][0]["question"],
            "Which platform must this run on?"
        );
        let record = last_record(dir.path());
        assert!(record.get("name").is_none(), "no brief, no name");
        assert_eq!(record["guidance"]["questions"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn below_threshold_answers_fold_into_the_brief_idea_and_rescore() {
        let dir = tempfile::tempdir().unwrap();
        // assess (low) -> re-assess after answers (high) -> brief.
        let provider = FakeProvider::new()
            .text(LOW_GUIDANCE)
            .text(HIGH_GUIDANCE)
            .text(VALID_BRIEF);
        let mut out = Vec::new();
        let mut answers = StdinAsker(std::io::Cursor::new(
            b"Windows desktop\nSQLite file\n".to_vec(),
        ));
        let gate = GuidanceGate {
            threshold: 0.7,
            max_questions: 5,
            clarification: Clarification::Ask(&mut answers),
        };
        let outcome = intake_flow(
            dir.path(),
            &provider,
            "m",
            "build a widget",
            Some(gate),
            &mut out,
        )
        .await
        .unwrap();
        assert_eq!(outcome, IntakeOutcome::BriefWritten);

        let prompts = String::from_utf8(out).unwrap();
        assert!(prompts.contains("Which platform must this run on?"));
        assert!(prompts.contains("Where is widget state stored?"));

        // The brief request carries the user's decisions, so the brief stands
        // without the transcript.
        let requests = provider.requests();
        let brief_request = requests.last().unwrap();
        let user_text = brief_request
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                localpilot_core::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(user_text.contains("Decisions provided by the user"));
        assert!(user_text.contains("platform: Windows desktop"));

        let record = last_record(dir.path());
        assert_eq!(record["guidance"]["answers"].as_array().unwrap().len(), 2);
        assert!((record["guidance"]["rescore"].as_f64().unwrap() - 1.0).abs() < 1e-6);
        assert!(record["guidance"].get("assumed_judgment").is_none());
    }

    #[tokio::test]
    async fn below_threshold_all_empty_answers_delegate_judgment() {
        let dir = tempfile::tempdir().unwrap();
        // assess (low) -> brief; no re-assessment when nothing was answered.
        let provider = FakeProvider::new().text(LOW_GUIDANCE).text(VALID_BRIEF);
        let mut out = Vec::new();
        let mut answers = StdinAsker(std::io::Cursor::new(b"\n\n".to_vec()));
        let gate = GuidanceGate {
            threshold: 0.7,
            max_questions: 5,
            clarification: Clarification::Ask(&mut answers),
        };
        let outcome = intake_flow(
            dir.path(),
            &provider,
            "m",
            "build a widget",
            Some(gate),
            &mut out,
        )
        .await
        .unwrap();
        assert_eq!(outcome, IntakeOutcome::BriefWritten);
        let record = last_record(dir.path());
        assert_eq!(record["guidance"]["assumed_judgment"], true);
        assert!(record["guidance"].get("rescore").is_none());
    }

    #[tokio::test]
    async fn assume_judgment_proceeds_and_records_the_delegation() {
        let dir = tempfile::tempdir().unwrap();
        let provider = FakeProvider::new().text(LOW_GUIDANCE).text(VALID_BRIEF);
        let mut out = Vec::new();
        let gate = GuidanceGate {
            threshold: 0.7,
            max_questions: 5,
            clarification: Clarification::AssumeJudgment,
        };
        let outcome = intake_flow(
            dir.path(),
            &provider,
            "m",
            "build a widget",
            Some(gate),
            &mut out,
        )
        .await
        .unwrap();
        assert_eq!(outcome, IntakeOutcome::BriefWritten);
        assert!(dir.path().join("brief.md").exists());
        let record = last_record(dir.path());
        assert_eq!(record["guidance"]["assumed_judgment"], true);
    }

    #[tokio::test]
    async fn a_score_exactly_at_the_threshold_proceeds() {
        let dir = tempfile::tempdir().unwrap();
        // LOW_GUIDANCE resolves 1 of 3 axes; the same fraction as the
        // threshold must pass (>= semantics, not >).
        let provider = FakeProvider::new().text(LOW_GUIDANCE).text(VALID_BRIEF);
        let mut out = Vec::new();
        let gate = GuidanceGate {
            threshold: 1.0 / 3.0,
            max_questions: 5,
            clarification: Clarification::Emit,
        };
        let outcome = intake_flow(
            dir.path(),
            &provider,
            "m",
            "build a widget",
            Some(gate),
            &mut out,
        )
        .await
        .unwrap();
        assert_eq!(outcome, IntakeOutcome::BriefWritten);
    }

    #[tokio::test]
    async fn golden_pair_ambiguous_pauses_and_specified_proceeds() {
        // The pause-and-ask contract, asserted from the run artifacts: an
        // ambiguous idea leaves no brief and a record whose guidance block
        // has questions but no brief name; a well-specified idea proceeds
        // straight to a named brief. The intake record is the
        // clarified-before-brief signal.
        let dir = tempfile::tempdir().unwrap();
        let provider = FakeProvider::new()
            .text(LOW_GUIDANCE)
            .text(HIGH_GUIDANCE)
            .text(VALID_BRIEF);
        let mut out = Vec::new();
        let gate = GuidanceGate {
            threshold: 0.7,
            max_questions: 5,
            clarification: Clarification::Emit,
        };
        let paused = intake_flow(
            dir.path(),
            &provider,
            "m",
            "make the widget better",
            Some(gate),
            &mut out,
        )
        .await
        .unwrap();
        assert_eq!(paused, IntakeOutcome::NeedsGuidance);
        assert!(!dir.path().join("brief.md").exists());
        let paused_record = last_record(dir.path());
        assert!(paused_record.get("name").is_none());
        assert!(paused_record["guidance"]["questions"].as_array().is_some());

        let gate = GuidanceGate {
            threshold: 0.7,
            max_questions: 5,
            clarification: Clarification::Emit,
        };
        let mut out = Vec::new();
        let proceeded = intake_flow(
            dir.path(),
            &provider,
            "m",
            "build the widget on Windows",
            Some(gate),
            &mut out,
        )
        .await
        .unwrap();
        assert_eq!(proceeded, IntakeOutcome::BriefWritten);
        assert!(dir.path().join("brief.md").exists());
        let written_record = last_record(dir.path());
        assert_eq!(written_record["name"], "widget");
        assert!(written_record["guidance"].get("questions").is_none());
    }

    #[tokio::test]
    async fn max_questions_caps_the_ask_list() {
        let dir = tempfile::tempdir().unwrap();
        let provider = FakeProvider::new().text(LOW_GUIDANCE);
        let mut out = Vec::new();
        let gate = GuidanceGate {
            threshold: 0.7,
            max_questions: 1,
            clarification: Clarification::Emit,
        };
        intake_flow(
            dir.path(),
            &provider,
            "m",
            "build a widget",
            Some(gate),
            &mut out,
        )
        .await
        .unwrap();
        let report: serde_json::Value = serde_json::from_slice(&out).unwrap();
        // Only the most consequential open axis is asked; the full axis list
        // stays in the report for inspection.
        assert_eq!(report["open"].as_array().unwrap().len(), 1);
        assert_eq!(report["axes"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn status_render_is_stable() {
        let report = StatusReport {
            branch: Some("feature/parser-errors".to_string()),
            next_step: Some("2. Implement parser errors".to_string()),
            completed: 1,
            total: 3,
            dirty: false,
            test_command: Some("cargo test".to_string()),
            default_provider: "local".to_string(),
            provider_credential_present: false,
            gate: vec!["fmt (step)".to_string(), "test (phase)".to_string()],
            lifecycle: "plan current".to_string(),
        };
        insta::assert_snapshot!(report.render());
    }

    #[test]
    fn default_config_has_no_dangling_default_provider() {
        // The starter config must stay internally consistent: an active
        // `default = "..."` line requires an active `[providers.<id>]` table.
        // Shipping `default = "local"` with the provider commented out made the
        // first `ask`/`print`/`chat` fail to resolve a provider.
        let active = |prefix: &str| {
            DEFAULT_CONFIG.lines().any(|line| {
                let trimmed = line.trim_start();
                !trimmed.starts_with('#') && trimmed.starts_with(prefix)
            })
        };
        assert_eq!(
            active("default ="),
            active("[providers."),
            "starter config: an active default provider must have an active [providers.*] table"
        );
    }
}
