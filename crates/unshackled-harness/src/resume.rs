//! `harness resume`: run the next plan step end to end — work the step through
//! the session loop, run configured tests, evaluate the completion rules, then
//! commit the step and the progress update.

use std::path::Path;
use std::process::Command;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use unshackled_config::CheckConfig;
use unshackled_llm::QuotaInfo;
use unshackled_quota::{estimate_window, PausedRun};

use crate::decisions::{today, Decisions};
use crate::error::HarnessError;
use crate::progress::{Progress, Step};
use crate::rules::{RuleEngine, Trigger};
use crate::session::{RuntimeEvent, SessionRuntime, StopReason};
use crate::worker::{
    decide_step, AttemptResult, CompletionInputs, StepAction, StepDecision, StepLoop,
};

const WORKER_PROMPT: &str = "\
You are completing exactly one step of an implementation plan. Make the change \
using the available tools, then briefly confirm completion. Do not start any other \
step.\n\nStep: ";

/// The store key under which a paused run is persisted (an inspectable file
/// under `.unshackled/cache/`).
pub const QUOTA_PAUSE_KEY: &str = "quota-paused.json";

/// The outcome of attempting one step via resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeOutcome {
    pub step_number: usize,
    pub committed: bool,
    pub blocked_reason: Option<String>,
    /// Whether the run paused on a provider quota/rate limit; a `PausedRun` was
    /// persisted and `harness wait-resume` can continue it.
    pub paused: bool,
}

/// Cap on automated replans within a single step, so the act-on-findings loop
/// records the deviation and halts instead of looping the planner forever
/// (anti-sunk-cost §6). One replan is enough to surface a stuck step to the
/// human or the next `plan --replan` run.
const MAX_REPLANS: u32 = 1;

/// Run the next incomplete step: work it, test it, run the quality gate, and act
/// on the findings — auto-fixes already ran inside the gate, remaining failures
/// feed the reason back to the model bounded by `max_attempts`, then a replan is
/// recorded to `DECISIONS.md`. On a clean pass, commit the step and the progress
/// update.
///
/// # Errors
/// Returns [`HarnessError`] if the project files cannot be read/written or git
/// operations fail.
pub async fn resume_one_step(
    runtime: &mut SessionRuntime,
    root: &Path,
    rule_engine: &RuleEngine,
    test_command: Option<&str>,
    checks: &[CheckConfig],
    max_attempts: u32,
) -> Result<ResumeOutcome, HarnessError> {
    let progress_path = root.join("PROGRESS.md");
    let mut progress = Progress::parse(&read(&progress_path)?)?;
    let step = progress
        .next_incomplete()
        .ok_or_else(|| HarnessError::Malformed {
            document: "PROGRESS.md",
            detail: "no incomplete steps remain".to_string(),
        })?
        .clone();

    let (events, _rx) = broadcast::channel::<RuntimeEvent>(256);
    let cancel = CancellationToken::new();
    let commit_message = format!("harness: {}", step.description);

    // The anti-sunk-cost loop owns the attempt/replan budget; each pass works the
    // step, runs the step-cadence gate, and turns the findings into a verdict.
    let mut step_loop = StepLoop::new(max_attempts.max(1), MAX_REPLANS);
    let mut prompt = format!("{WORKER_PROMPT}{}. {}", step.number, step.description);

    loop {
        let reason = runtime.run_turn(&prompt, &events, &cancel).await;

        // A provider quota/rate error pauses the run cleanly at this step
        // boundary: persist an inspectable PausedRun and stop without committing.
        if reason == StopReason::ProviderError {
            // Prefer the provider's own quota metadata (retry-after, limit kind)
            // so the pause window is precise; fall back to a conservative
            // retryable default when the error carried none.
            let quota = runtime.last_quota().cloned().unwrap_or(QuotaInfo {
                retryable: true,
                ..QuotaInfo::default()
            });
            let window = estimate_window(&quota, 1);
            let paused = PausedRun::new(step.number, "provider", &window);
            if let Ok(json) = serde_json::to_string(&paused) {
                let _ = runtime.store().put_cache(QUOTA_PAUSE_KEY, json.as_bytes());
            }
            return Ok(ResumeOutcome {
                step_number: step.number,
                committed: false,
                blocked_reason: Some(format!("paused on provider limit: {}", window.reason)),
                paused: true,
            });
        }
        // Any other non-completing turn must not commit the step.
        if reason != StopReason::Done {
            return Ok(ResumeOutcome {
                step_number: step.number,
                committed: false,
                blocked_reason: Some(format!("turn did not complete ({reason:?})")),
                paused: false,
            });
        }

        // Run configured tests (`suite_green`) and the step-cadence quality gate,
        // then reduce both to a single action.
        let tests_passed = test_command.map(|cmd| run_test_command(root, cmd));
        let gate_outcomes = runtime
            .run_gate_checks(checks, Trigger::StepComplete, root)
            .await;
        let action = decide_step(
            rule_engine,
            &CompletionInputs {
                tests_passed,
                progress_reflects_completion: true,
                commit_message: commit_message.clone(),
                attempts: 1,
                max_attempts,
            },
            gate_outcomes,
        );

        match action {
            StepAction::Commit => break,
            // A blocking finding (audit/dependency, a failing test, a dirty commit
            // message) needs a human or dependency decision — never a retry.
            StepAction::Block(blocked_reason) => {
                return Ok(ResumeOutcome {
                    step_number: step.number,
                    committed: false,
                    blocked_reason: Some(blocked_reason),
                    paused: false,
                });
            }
            // An actionable finding: feed it back through the anti-sunk-cost loop.
            StepAction::Retry(reason) => match step_loop.on_attempt(AttemptResult::Retry(reason)) {
                StepDecision::RetrySameContext(feedback)
                | StepDecision::DiscardAndReset(feedback) => {
                    prompt = retry_prompt(&step, &feedback);
                }
                StepDecision::Replan(logs) => {
                    record_replan(root, &progress.name, step.number, &logs)?;
                    return Ok(ResumeOutcome {
                        step_number: step.number,
                        committed: false,
                        blocked_reason: Some(format!(
                            "replanned after {} failed attempts; recorded in DECISIONS.md",
                            logs.len()
                        )),
                        paused: false,
                    });
                }
                StepDecision::GiveUp => {
                    return Ok(ResumeOutcome {
                        step_number: step.number,
                        committed: false,
                        blocked_reason: Some(
                            "gave up: the replan cap was reached for this step".to_string(),
                        ),
                        paused: false,
                    });
                }
                // `on_attempt` only returns `Commit` for a `Success` result, which
                // this loop never feeds; treat it as a pass for completeness.
                StepDecision::Commit => break,
            },
        }
    }

    // Commit the step.
    git(root, &["add", "-A"])?;
    git(root, &["commit", "-m", &commit_message])?;
    let hash = git(root, &["rev-parse", "--short", "HEAD"])?
        .trim()
        .to_string();

    // Update and commit progress.
    progress.mark_complete(step.number, Some(hash), step_loop.replans() + 1);
    write(&progress_path, &progress.render())?;
    git(root, &["add", "PROGRESS.md"])?;
    git(root, &["commit", "-m", "harness: update progress"])?;

    Ok(ResumeOutcome {
        step_number: step.number,
        committed: true,
        blocked_reason: None,
        paused: false,
    })
}

/// Re-issue the step prompt with the gate's feedback appended, so the next
/// attempt sees exactly what to fix. The runtime keeps the prior conversation,
/// so this is the keep-context retry the anti-sunk-cost loop intends.
fn retry_prompt(step: &Step, feedback: &str) -> String {
    format!(
        "{WORKER_PROMPT}{}. {}\n\nThe previous attempt did not pass the quality gate: {feedback}. \
         Address the findings and complete the step.",
        step.number, step.description
    )
}

/// Append a replan entry to `DECISIONS.md` (creating it on first deviation), so
/// the reason a step was abandoned survives a context reset.
fn record_replan(
    root: &Path,
    name: &str,
    step_number: usize,
    logs: &[String],
) -> Result<(), HarnessError> {
    let path = root.join("DECISIONS.md");
    let mut decisions = match std::fs::read_to_string(&path) {
        Ok(text) => Decisions::parse(&text)?,
        Err(_) => Decisions::new(name),
    };
    let rationale = if logs.is_empty() {
        "the per-step attempt budget was exhausted".to_string()
    } else {
        format!("attempts failed: {}", logs.join("; "))
    };
    decisions.append(
        today(),
        format!("Replan step {step_number}"),
        "the automated attempt budget was exhausted; the step is queued for replanning",
        rationale,
        format!("step {step_number}"),
    );
    write(&path, &decisions.render())
}

fn run_test_command(root: &Path, command: &str) -> bool {
    let mut parts = command.split_whitespace();
    let Some(program) = parts.next() else {
        return false;
    };
    Command::new(program)
        .args(parts)
        .current_dir(root)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn git(root: &Path, args: &[&str]) -> Result<String, HarnessError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| HarnessError::Provider(format!("git: {e}")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(HarnessError::Provider(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

fn read(path: &Path) -> Result<String, HarnessError> {
    std::fs::read_to_string(path).map_err(|source| HarnessError::Io {
        path: path.display().to_string(),
        source,
    })
}

fn write(path: &Path, contents: &str) -> Result<(), HarnessError> {
    std::fs::write(path, contents).map_err(|source| HarnessError::Io {
        path: path.display().to_string(),
        source,
    })
}
