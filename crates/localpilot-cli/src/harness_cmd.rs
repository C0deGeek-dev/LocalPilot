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
    summarize_proposal, Brief, CheckOutcome, CheckStatus, Progress, RuleEngine, RuntimeEvent,
    SessionConfig, SessionRuntime, QUALITY_CHECK_TOOL, QUOTA_PAUSE_KEY,
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

    let progress = std::fs::read_to_string(root.join("PROGRESS.md"))
        .ok()
        .and_then(|text| Progress::parse(&text).ok());
    let (next_step, completed, total) = match &progress {
        Some(p) => (
            p.next_incomplete()
                .map(|s| format!("{}. {}", s.number, s.description)),
            p.completed_count(),
            p.steps.len(),
        ),
        None => (None, 0, 0),
    };

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
    })
}

fn cadence_label(cadence: localpilot_config::Cadence) -> &'static str {
    match cadence {
        localpilot_config::Cadence::Step => "step",
        localpilot_config::Cadence::Phase => "phase",
    }
}

fn git_line(root: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
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

/// How a below-threshold guidance assessment resolves the open decisions.
pub enum Clarification<'a> {
    /// Interactive surface: ask each open question on `out` and read one
    /// answer per line from the reader; an empty answer delegates that axis
    /// to the model's judgment.
    Ask(&'a mut dyn std::io::BufRead),
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
    let mut stdin_lines;
    let clarification = if assume_judgment {
        Clarification::AssumeJudgment
    } else if interactive {
        stdin_lines = std::io::BufReader::new(std::io::stdin());
        Clarification::Ask(&mut stdin_lines)
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
        Clarification::Ask(input) => {
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
                write!(out, "\n{question}\n> ")?;
                out.flush()?;
                let mut answer = String::new();
                input.read_line(&mut answer)?;
                let answer = answer.trim().to_string();
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
    let progress = run_plan(provider.as_ref(), model, &brief, &summary).await?;
    std::fs::write(root.join("PROGRESS.md"), progress.render())?;
    Ok(())
}

/// Add a feature to an existing brief and plan, without renumbering completed
/// steps. This is deterministic and needs no provider.
///
/// # Errors
/// Returns an error if the brief or progress files are missing or invalid.
pub fn feature(root: &Path, description: &str) -> anyhow::Result<()> {
    let mut brief = Brief::parse(&std::fs::read_to_string(root.join("brief.md"))?)?;
    brief.add_requirement(description);
    std::fs::write(root.join("brief.md"), brief.render())?;

    let mut progress = Progress::parse(&std::fs::read_to_string(root.join("PROGRESS.md"))?)?;
    progress.append_step(format!("Implement: {description}"));
    std::fs::write(root.join("PROGRESS.md"), progress.render())?;
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
) -> anyhow::Result<()>
where
    A: FnMut() -> Box<dyn Approver>,
{
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

    const MAX_STEPS: usize = 100;
    for _ in 0..MAX_STEPS {
        let next_step = std::fs::read_to_string(root.join("PROGRESS.md"))
            .ok()
            .and_then(|t| Progress::parse(&t).ok())
            .and_then(|p| p.next_incomplete().map(|s| s.description.clone()));
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
            writeln!(out, "step {} complete", outcome.step_number)?;
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
    Ok(())
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
) -> anyhow::Result<()>
where
    A: FnMut() -> Box<dyn Approver>,
{
    let store = Store::open(root);
    let Some(bytes) = store.get_cache(QUOTA_PAUSE_KEY)? else {
        writeln!(out, "no paused run")?;
        return Ok(());
    };
    let paused: PausedRun = serde_json::from_slice(&bytes)
        .map_err(|e| anyhow::anyhow!("invalid paused-run file: {e}"))?;

    let config = localpilot_config::load(&ConfigPaths::standard(root), &CliOverrides::default())
        .unwrap_or_else(|_| Config::default());
    let policy = ResumePolicy::from(&config.quota);
    let now = now_unix();
    let ctx = ResumeContext {
        window_elapsed: paused.resume_eligible_unix.is_none_or(|t| now >= t),
        at_step_boundary: true,
        workspace_clean: !workspace_dirty(root),
        pending_destructive_approval: false,
        user_cancelled: false,
        provider_identity_changed: false,
        waited: Duration::from_secs(now.saturating_sub(paused.paused_at_unix)),
    };

    match decide_resume(&policy, &ctx) {
        ResumeDecision::Resume => {
            writeln!(out, "resuming paused run at step {}", paused.step_number)?;
            store.delete_cache(QUOTA_PAUSE_KEY)?;
            resume_with_events(root, model, provider_id, run, events, cancel, out).await?;
        }
        ResumeDecision::Wait => {
            let eta = paused
                .resume_eligible_unix
                .map_or(0, |t| t.saturating_sub(now));
            writeln!(
                out,
                "paused ({}); resume eligible in ~{eta}s",
                paused.reason
            )?;
        }
        ResumeDecision::AskUser => {
            writeln!(
                out,
                "auto_resume is 'ask'; set quota.auto_resume = run|global to continue automatically"
            )?;
        }
        ResumeDecision::BlockedBy(reason) => {
            writeln!(out, "cannot resume: {reason}")?;
        }
    }
    Ok(())
}

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
            ..SessionConfig::default()
        },
        Vec::new(),
    );
    runtime.set_broker(broker);
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
        let mut answers = std::io::Cursor::new(b"Windows desktop\nSQLite file\n".to_vec());
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
        let mut answers = std::io::Cursor::new(b"\n\n".to_vec());
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
