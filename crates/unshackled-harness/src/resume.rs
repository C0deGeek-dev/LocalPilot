//! `harness resume`: run the next plan step end to end — work the step through
//! the session loop, run configured tests, evaluate the completion rules, then
//! commit the step and the progress update.

use std::path::Path;
use std::process::Command;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use unshackled_llm::QuotaInfo;
use unshackled_quota::{estimate_window, PausedRun};

use crate::error::HarnessError;
use crate::progress::Progress;
use crate::rules::RuleEngine;
use crate::session::{RuntimeEvent, SessionRuntime, StopReason};
use crate::worker::{evaluate_completion, CompletionDecision, CompletionInputs};

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

/// Run the next incomplete step: work it, test it, gate it through the rules,
/// then commit the step and the progress update.
///
/// # Errors
/// Returns [`HarnessError`] if the project files cannot be read/written or git
/// operations fail.
pub async fn resume_one_step(
    runtime: &mut SessionRuntime,
    root: &Path,
    rule_engine: &RuleEngine,
    test_command: Option<&str>,
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

    // Work the step through the session loop.
    let (events, _rx) = broadcast::channel::<RuntimeEvent>(256);
    let cancel = CancellationToken::new();
    let prompt = format!("{WORKER_PROMPT}{}. {}", step.number, step.description);
    let reason = runtime.run_turn(&prompt, &events, &cancel).await;

    // A provider quota/rate error pauses the run cleanly at this step boundary:
    // persist an inspectable PausedRun and stop without committing.
    if reason == StopReason::ProviderError {
        let window = estimate_window(
            &QuotaInfo {
                retryable: true,
                ..QuotaInfo::default()
            },
            1,
        );
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

    // Run configured tests.
    let tests_passed = test_command.map(|cmd| run_test_command(root, cmd));

    // Gate completion through the rules.
    let commit_message = format!("harness: {}", step.description);
    let decision = evaluate_completion(
        rule_engine,
        &CompletionInputs {
            tests_passed,
            progress_reflects_completion: true,
            commit_message: commit_message.clone(),
            attempts: 1,
            max_attempts,
        },
    );
    if let CompletionDecision::Blocked(reason) = decision {
        return Ok(ResumeOutcome {
            step_number: step.number,
            committed: false,
            blocked_reason: Some(reason),
            paused: false,
        });
    }

    // Commit the step.
    git(root, &["add", "-A"])?;
    git(root, &["commit", "-m", &commit_message])?;
    let hash = git(root, &["rev-parse", "--short", "HEAD"])?
        .trim()
        .to_string();

    // Update and commit progress.
    progress.mark_complete(step.number, Some(hash), 1);
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
