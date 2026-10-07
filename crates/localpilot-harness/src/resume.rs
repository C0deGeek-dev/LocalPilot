//! `harness resume`: run the next plan step end to end — work the step through
//! the session loop, run configured tests, evaluate the completion rules, then
//! commit the step and the progress update.

use std::io::Read;
use std::path::Path;
use std::process::Command;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use localpilot_config::{AutoFix, Cadence, CheckConfig};
use localpilot_core::StructuredSummary;
use localpilot_llm::QuotaInfo;
use localpilot_quota::{estimate_window, PausedRun};
use localpilot_store::SessionEventKind;
use sha2::{Digest, Sha256};

use crate::decisions::{today, Decisions};
use crate::error::HarnessError;
use crate::progress::{Progress, Step};
use crate::quality::CheckOutcome;
use crate::rules::{RuleContext, RuleEngine, RuleVerdict, Trigger};
use crate::session::{RuntimeEvent, SessionRuntime, StopReason};
use crate::worker::{
    decide_step, AttemptResult, CompletionInputs, StepAction, StepDecision, StepLoop,
};

const WORKER_PROMPT: &str = "\
You are completing exactly one step of an implementation plan. Make the change \
using the available tools, then briefly confirm completion. If the change alters \
observable behaviour, configuration, or interfaces, update the matching \
documentation in a bounded follow-up step in this plan before completing the feature. Do not start any other step. \
Do not edit PROGRESS.md: the harness marks the step complete and commits it.\n\nStep: ";

/// The store key under which a paused run is persisted (an inspectable file
/// under `.localpilot/cache/`).
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
    /// The quality-gate outcomes from the deciding attempt (which checks ran,
    /// pass/fail, what was auto-fixed). Empty when the step ended before the gate
    /// ran (a paused or non-completing turn).
    pub gate: Vec<CheckOutcome>,
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
    let (events, _rx) = broadcast::channel::<RuntimeEvent>(256);
    let cancel = CancellationToken::new();
    resume_one_step_with_events(
        runtime,
        root,
        rule_engine,
        test_command,
        checks,
        max_attempts,
        &events,
        &cancel,
    )
    .await
}

/// Run the next incomplete step while streaming runtime events to `events` and
/// honoring `cancel`. This is the same harness loop as [`resume_one_step`], but
/// host UIs can subscribe to the event stream instead of waiting for the final
/// outcome.
///
/// # Errors
/// Returns [`HarnessError`] if the project files cannot be read/written or git
/// operations fail.
#[allow(clippy::too_many_arguments)] // the host owns event/cancel wiring
pub async fn resume_one_step_with_events(
    runtime: &mut SessionRuntime,
    root: &Path,
    rule_engine: &RuleEngine,
    test_command: Option<&str>,
    checks: &[CheckConfig],
    max_attempts: u32,
    events: &broadcast::Sender<RuntimeEvent>,
    cancel: &CancellationToken,
) -> Result<ResumeOutcome, HarnessError> {
    // The gate lives in the executor, not only in the callers. This function is
    // public, so a library consumer reaching it directly must meet the same
    // conditions as the CLI and the interactive host: an unbound, stale, or
    // unreadable plan cannot execute merely because the caller skipped the
    // check. It also replaces a misleading error — a completed plan used to
    // report `PROGRESS.md is malformed: no incomplete steps remain`, which is a
    // claim about the document rather than about the work.
    let state = crate::workspace_state::inspect(crate::workspace_state::WorkspaceInputs::at(root));
    let progress = crate::workspace_state::resumable(&state)?;
    let step = progress
        .next_incomplete()
        .ok_or(crate::workspace_state::NotResumable::Complete)?
        .clone();
    // Before the baseline is recorded: the baseline is a commit, and the plan
    // documents must be part of it rather than counted as the step's own work.
    commit_plan_documents(root)?;
    let work_baseline = runtime
        .work_profile()
        .and_then(|_| work_diff_baseline(root).ok());
    runtime.set_harness_checkpoint_owner(step.scope);
    let progress_path = root.join("PROGRESS.md");
    let mut progress = progress.clone();

    let session_start_ctx = RuleContext {
        uncommitted_unrelated: has_unrelated_uncommitted_changes(root)?,
        ..RuleContext::default()
    };
    if let Some(reason) =
        first_blocking_reason(rule_engine, Trigger::SessionStart, &session_start_ctx)
    {
        return Ok(ResumeOutcome {
            step_number: step.number,
            committed: false,
            blocked_reason: Some(reason),
            paused: false,
            gate: Vec::new(),
        });
    }

    let commit_message = format!("harness: {}", step.description);

    // The step is a branch in the event tree: attempts chain from here, an
    // abandoned attempt closes with a structured summary, and a fresh attempt
    // forks back to this anchor.
    runtime.record_event(SessionEventKind::StepStarted {
        number: step.number,
        description: step.description.clone(),
    });
    let step_anchor = runtime.last_event_id();
    // Link this session to the step before any exit path, so a pause or a block
    // here still leaves the session findable once the step commits later.
    crate::step_sessions::note(
        runtime.store(),
        step.number,
        &step.description,
        runtime.session_id(),
    );

    // The anti-sunk-cost loop owns the attempt/replan budget; each pass works the
    // step, runs the step-cadence gate, and turns the findings into a verdict.
    let mut step_loop = StepLoop::new(max_attempts.max(1), MAX_REPLANS);
    let mut prompt = format!("{WORKER_PROMPT}{}. {}", step.number, step.description);
    if let Some(profile) = runtime.work_profile() {
        if let Some(verification) = &step.verify {
            runtime.set_work_unit_verification(verification);
        }
        if step.scope.is_some_and(|scope| !profile.accepts(scope)) {
            return Ok(ResumeOutcome {
                step_number: step.number, committed: false, paused: false, gate: Vec::new(),
                blocked_reason: Some("step exceeds the effective work envelope; replan and split it while preserving coverage and dependencies".to_string()),
            });
        }
        prompt.push_str(&format!("\n\n{}", profile.instruction()));
        if let Some(crate::Verification::Command(command)) = &step.verify {
            prompt.push_str(&format!(
                "\nSmallest planned verification: {command}. The full ratified gate still applies."
            ));
        }
    }
    // The deciding attempt's gate outcomes, surfaced on the returned outcome.
    // Assigned on every loop pass before any exit that reads it.
    let mut final_gate: Vec<CheckOutcome>;

    loop {
        let reason = runtime.run_turn(&prompt, events, cancel).await;

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
            // Escalate the backoff across repeated pauses: read the prior marker's
            // attempt (if this step already paused) and grow it, so a provider that
            // keeps limiting is retried with an ever-wider window rather than the
            // same one. A provider-stated window (retry_after/reset_at) ignores the
            // attempt; only the fallback backoff widens.
            let attempt = runtime
                .store()
                .get_cache(QUOTA_PAUSE_KEY)
                .ok()
                .flatten()
                .and_then(|bytes| serde_json::from_slice::<PausedRun>(&bytes).ok())
                .map_or(1, |prev| prev.attempt.saturating_add(1));
            let window = estimate_window(&quota, attempt);
            let paused = PausedRun::new(step.number, runtime.active_provider_id(), &window)
                .with_attempt(attempt);
            // A failed marker write must be visible: without the marker a later
            // `resume` can't see the pause window and retries into the same
            // limit. The pause itself still proceeds (the outcome carries the
            // reason) — only silence is unacceptable.
            match serde_json::to_string(&paused) {
                Ok(json) => {
                    if let Err(error) = runtime.store().put_cache(QUOTA_PAUSE_KEY, json.as_bytes())
                    {
                        tracing::warn!(
                            target: "localpilot::harness",
                            %error,
                            "could not persist the quota pause marker; a later resume will not see this pause window"
                        );
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        target: "localpilot::harness",
                        %error,
                        "could not serialize the quota pause marker; a later resume will not see this pause window"
                    );
                }
            }
            return Ok(ResumeOutcome {
                step_number: step.number,
                committed: false,
                blocked_reason: Some(format!("paused on provider limit: {}", window.reason)),
                paused: true,
                gate: Vec::new(),
            });
        }
        // Any other non-completing turn must not commit the step.
        if reason != StopReason::Done {
            return Ok(ResumeOutcome {
                step_number: step.number,
                committed: false,
                blocked_reason: Some(format!("turn did not complete ({reason:?})")),
                paused: false,
                gate: Vec::new(),
            });
        }

        // Argument sizing cannot establish the footprint of shell/MCP writes.
        // Inspect the actual diff before any quality auto-fix or commit. Refusal
        // keeps the work and session evidence for an explicit split/review.
        if let Some(profile) = runtime.work_profile() {
            let inspection = inspect_work_diff(root, profile, work_baseline.as_ref(), true)?;
            inspection.record(runtime, events);
            if let Some(reason) = inspection.blocked {
                return Ok(ResumeOutcome {
                    step_number: step.number,
                    committed: false,
                    blocked_reason: Some(reason),
                    paused: false,
                    gate: Vec::new(),
                });
            }
        }

        // Run configured tests (`suite_green`) and the step-cadence quality gate,
        // then reduce both to a single action.
        let mut step_checks = Vec::new();
        if let Some(check) = test_command.and_then(legacy_test_check) {
            step_checks.push(check);
        }
        step_checks.extend(checks.iter().cloned());
        final_gate = runtime
            .run_gate_checks(&step_checks, Trigger::StepComplete, root)
            .await;
        let tests_passed = final_gate
            .iter()
            .find(|outcome| outcome.name == "test")
            .map(CheckOutcome::passed);
        // Re-read PROGRESS.md after the turn: did the model actually tick the
        // step it claimed to complete? A best-effort re-read that cannot confirm
        // the update leaves the (advisory) progress rule to flag it.
        let progress_reflects_completion = read(&progress_path)
            .ok()
            .and_then(|raw| Progress::parse(&raw).ok())
            .is_some_and(|updated| updated.step_is_done(step.number));
        // The real per-step attempt/replan budget is owned by `step_loop`
        // (`StepLoop`, above), which turns exhaustion into a bounded retry →
        // discard → replan → give-up. The completion gate here judges only the
        // step's *outcome* (tests, progress, commit message, quality), so the
        // `attempt_limit` rule is intentionally fed a fixed `attempts = 1`: on
        // this path it is a documented redundancy, not the enforcer (see
        // `docs/06-harness-spec.md`, "Runtime status"). It is left as-is rather
        // than dropped because with a configured `attempts_per_step = 1` a fed
        // `attempts = max_attempts` would let the rule block an otherwise
        // passing step — a behaviour change out of scope for a doc/dead-surface
        // pass.
        let action = decide_step(
            rule_engine,
            &CompletionInputs {
                tests_passed,
                progress_reflects_completion,
                commit_message: commit_message.clone(),
                attempts: 1,
                max_attempts,
            },
            final_gate.clone(),
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
                    gate: final_gate,
                });
            }
            // An actionable finding: feed it back through the anti-sunk-cost
            // loop. A `discard` action abandons the attempt outright — the
            // loop then answers `DiscardAndReset`, which restores committed
            // state before the fresh attempt (the spec's discard rung).
            StepAction::Retry(_) | StepAction::Discard(_) => {
                match step_loop.on_attempt(match action {
                    StepAction::Retry(reason) => AttemptResult::Retry(reason),
                    StepAction::Discard(reason) => AttemptResult::Discard(reason),
                    // Unreachable: the outer match arm only binds Retry/Discard.
                    _ => AttemptResult::Success,
                }) {
                    StepDecision::RetrySameContext(feedback) => {
                        prompt = retry_prompt(&step, &feedback);
                    }
                    StepDecision::DiscardAndReset(feedback) => {
                        // Restore committed state FIRST — the discarded attempt's
                        // edits must not leak into the fresh attempt — then close
                        // the branch with a digest of what failed and fork from
                        // the step anchor, so the discarded line stays auditable.
                        restore_committed_state(root)?;
                        runtime.record_event(SessionEventKind::BranchClosed {
                            summary: StructuredSummary::new(
                                "Step attempt abandoned (working tree restored):",
                                vec![feedback.clone()],
                            ),
                        });
                        if let Some(from) = step_anchor {
                            runtime.record_event(SessionEventKind::BranchForked { from });
                        }
                        prompt = retry_prompt(&step, &feedback);
                    }
                    StepDecision::Replan(logs) => {
                        runtime.record_event(SessionEventKind::BranchClosed {
                            summary: StructuredSummary::new(
                                "Step attempt abandoned:",
                                logs.clone(),
                            ),
                        });
                        record_replan(root, &progress.name, step.number, &logs)?;
                        return Ok(ResumeOutcome {
                            step_number: step.number,
                            committed: false,
                            blocked_reason: Some(format!(
                                "replanned after {} failed attempts; recorded in DECISIONS.md",
                                logs.len()
                            )),
                            paused: false,
                            gate: final_gate,
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
                            gate: final_gate,
                        });
                    }
                    // `on_attempt` only returns `Commit` for a `Success` result, which
                    // this loop never feeds; treat it as a pass for completeness.
                    StepDecision::Commit => break,
                }
            }
        }
    }

    // Commit the step.
    if let Some(before) = &work_baseline {
        if git(root, &["rev-parse", "HEAD"])? != before.head {
            return Ok(ResumeOutcome { step_number: step.number, committed: false, paused: false,
                blocked_reason: Some("a command committed before the harness checkpoint; review that commit before resuming; no completion was recorded".to_string()), gate: final_gate });
        }
    }
    if let Some(profile) = runtime.work_profile() {
        let inspection = inspect_work_diff(root, profile, work_baseline.as_ref(), true)?;
        inspection.record(runtime, events);
        if let Some(reason) = inspection.blocked {
            return Ok(ResumeOutcome {
                step_number: step.number,
                committed: false,
                blocked_reason: Some(reason),
                paused: false,
                gate: final_gate,
            });
        }
    }
    let changed_paths = committable_status_paths(root)?
        .into_iter()
        .filter(|path| path != "PROGRESS.md")
        .collect::<Vec<_>>();
    let hash = if changed_paths.is_empty() {
        None
    } else {
        git_add_paths(root, &changed_paths)?;
        git(root, &["commit", "-m", &commit_message])?;
        Some(
            git(root, &["rev-parse", "--short", "HEAD"])?
                .trim()
                .to_string(),
        )
    };

    runtime.record_event(SessionEventKind::StepCompleted {
        number: step.number,
        commit: hash.clone(),
        attempts: step_loop.replans() + 1,
    });

    // Update and commit progress.
    progress.mark_complete(step.number, hash, step_loop.replans() + 1);
    progress.record_sessions(
        step.number,
        crate::step_sessions::collect(
            runtime.store(),
            step.number,
            &step.description,
            runtime.session_id(),
        ),
    );
    write(&progress_path, &progress.render())?;
    git(root, &["add", "PROGRESS.md"])?;
    git(root, &["commit", "-m", "harness: update progress"])?;
    crate::step_sessions::clear(runtime.store());

    // Phase-cadence quality gate. Steps carry a per-step gate (`StepComplete`);
    // phase-cadence checks — the expensive full-suite / dependency / audit set a
    // project ratifies with `cadence = "phase"` — evaluate on a phase boundary. In
    // this flat-step plan model the plan boundary is the phase boundary: once the
    // step just committed leaves no incomplete step, the phase checks run here,
    // once, rather than on every step. Without this the ratified phase checks
    // would never run, because the per-step gate only evaluates `StepComplete`.
    let mut gate = final_gate;
    let plan_complete = read(&progress_path)
        .ok()
        .and_then(|raw| Progress::parse(&raw).ok())
        .is_some_and(|updated| updated.next_incomplete().is_none());
    if plan_complete {
        let phase_outcomes = runtime
            .run_gate_checks(checks, Trigger::PhaseComplete, root)
            .await;
        if !phase_outcomes.is_empty() {
            // Reduce through the same `quality_gate` rule the step gate uses (it
            // triggers on both cadences). A blocking phase finding (e.g. a failing
            // `audit`) is surfaced to the human — the step stays committed, but the
            // run stops with the reason rather than reporting a clean completion.
            let phase_ctx = RuleContext {
                gate_outcomes: phase_outcomes.clone(),
                ..RuleContext::default()
            };
            let phase_block =
                first_blocking_reason(rule_engine, Trigger::PhaseComplete, &phase_ctx);
            gate.extend(phase_outcomes);
            if let Some(reason) = phase_block {
                return Ok(ResumeOutcome {
                    step_number: step.number,
                    committed: true,
                    blocked_reason: Some(format!("phase quality gate — {reason}")),
                    paused: false,
                    gate,
                });
            }
        }
    }

    Ok(ResumeOutcome {
        step_number: step.number,
        committed: true,
        blocked_reason: None,
        paused: false,
        gate,
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

fn legacy_test_check(command: &str) -> Option<CheckConfig> {
    let mut parts = command.split_whitespace();
    let program = parts.next()?.to_string();
    Some(CheckConfig {
        name: "test".to_string(),
        program,
        args: parts.map(str::to_string).collect(),
        fix_program: None,
        fix_args: Vec::new(),
        cadence: Cadence::Step,
        auto_fix: AutoFix::No,
        severity: None,
    })
}

/// Restore the working tree to committed state after a discarded attempt:
/// tracked files reset to `HEAD`, material untracked files the attempt created
/// removed. Use the same material path policy as inspection/commit, preserving
/// excluded generated paths and owned execution records even without Git ignore
/// rules. Resume refuses unrelated material before starting the attempt.
fn restore_committed_state(root: &Path) -> Result<(), HarnessError> {
    let paths = committable_status_paths(root)?;
    git(root, &["reset", "--hard", "HEAD"])?;
    let mut args = vec!["clean", "-fd", "--"];
    let mut argument_bytes = 0usize;
    for path in &paths {
        // Leave ample room below Windows' command-line limit, including quotes,
        // escaped backslashes and UTF-16 expansion. A rejected large attempt can
        // contain far more untracked material than an admitted work unit.
        let path_bytes = path.len().saturating_mul(2).saturating_add(3);
        if args.len() > 3 && argument_bytes.saturating_add(path_bytes) > 8192 {
            git(root, &args)?;
            args.truncate(3);
            argument_bytes = 0;
        }
        args.push(path.as_str());
        argument_bytes = argument_bytes.saturating_add(path_bytes);
    }
    if args.len() > 3 {
        git(root, &args)?;
    }
    Ok(())
}

fn git(root: &Path, args: &[&str]) -> Result<String, HarnessError> {
    let output = Command::new("git")
        // Status paths are literal file names, including Git pathspec syntax.
        .arg("--literal-pathspecs")
        .args(args)
        .current_dir(root)
        // `output()` already closes the child's stdin; kept explicit so a later
        // switch to `spawn()` cannot hand git the caller's stdin.
        .stdin(std::process::Stdio::null())
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

fn git_add_paths(root: &Path, paths: &[String]) -> Result<(), HarnessError> {
    let mut args = vec!["add", "--"];
    args.extend(paths.iter().map(String::as_str));
    git(root, &args).map(|_| ())
}

#[derive(Debug, Default)]
pub(crate) struct WorkDiffInspection {
    pub(crate) blocked: Option<String>,
    ignored_generated: Vec<String>,
    binary_changes: Vec<localpilot_store::BinaryChange>,
    binary_bytes: u64,
}

impl WorkDiffInspection {
    fn refuse(mut self, reason: impl Into<String>) -> Self {
        self.blocked = Some(reason.into());
        self
    }

    pub(crate) fn record(
        &self,
        runtime: &mut SessionRuntime,
        events: &broadcast::Sender<RuntimeEvent>,
    ) {
        runtime.record_event(SessionEventKind::WorkUnitInspected {
            ignored_generated: self.ignored_generated.clone(),
            binary_changes: self.binary_changes.clone(),
            binary_bytes: self.binary_bytes,
            blocked: self.blocked.clone(),
        });
        if !self.ignored_generated.is_empty() || !self.binary_changes.is_empty() {
            let notice = format!(
                "work unit inspection: excluded generated untracked paths {:?}; binary changes {:?} ({} old + new bytes / {}); verification obligations remain",
                self.ignored_generated,
                self.binary_changes,
                self.binary_bytes,
                crate::granularity::MAX_BINARY_CHANGE_BYTES,
            );
            let _ = events.send(RuntimeEvent::Warning(notice));
        }
    }
}

pub(crate) fn inspect_work_diff(
    root: &Path,
    profile: crate::granularity::WorkProfile,
    baseline: Option<&WorkDiffBaseline>,
    ignore_progress: bool,
) -> Result<WorkDiffInspection, HarnessError> {
    let base = baseline.map_or("HEAD", |before| before.head.trim());
    let (mut paths, ignored_generated) = work_status_paths(root)?;
    let mut inspection = WorkDiffInspection {
        ignored_generated,
        ..WorkDiffInspection::default()
    };
    paths.extend(
        git(
            root,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--name-only",
                "-z",
                "--no-renames",
                base,
                "--",
            ],
        )?
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_string),
    );
    paths.sort();
    paths.dedup();
    let mut changed = Vec::new();
    for path in paths {
        if is_runtime_state_path(&path) || (ignore_progress && path == "PROGRESS.md") {
            continue;
        }
        if let Some(old) = baseline.and_then(|before| before.files.get(&path)) {
            // An inspection failure is not evidence of unchanged content.
            if old == &work_fingerprint(root, &path)? {
                continue;
            }
        }
        changed.push(path);
    }
    let paths = changed;
    let mut lines = 0usize;
    let mut regions = 0usize;
    for path in &paths {
        let stat = git(
            root,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--numstat",
                "--no-renames",
                base,
                "--",
                path,
            ],
        )?;
        let binary_stat = stat.lines().any(|row| row.starts_with("-\t-\t"));
        if binary_stat {
            let before = baseline_blob(root, base, path)?;
            let after = working_material(root, path)?;
            let bytes = before
                .as_ref()
                .map_or(0, |m| m.bytes)
                .saturating_add(after.as_ref().map_or(0, |m| m.bytes));
            inspection.binary_bytes = inspection.binary_bytes.saturating_add(bytes);
            if inspection.binary_bytes > crate::granularity::MAX_BINARY_CHANGE_BYTES {
                let total = inspection.binary_bytes;
                return Ok(inspection.refuse(binary_budget_refusal(total)));
            }
            // Immutable baseline content and working bytes, never text decoding.
            let before = before.map(|m| m.finish()).transpose()?;
            let after = after.map(|m| m.finish()).transpose()?;
            if before == after {
                continue;
            }
            inspection
                .binary_changes
                .push(localpilot_store::BinaryChange {
                    path: path.clone(),
                    before,
                    after,
                });
            regions += 1;
        } else if stat.is_empty() {
            // New files have no tracked diff. Bound bytes before reading and
            // reject opaque/binary material rather than treating it as zero.
            let candidate = root.join(path);
            let metadata =
                std::fs::symlink_metadata(&candidate).map_err(|source| HarnessError::Io {
                    path: path.clone(),
                    source,
                })?;
            if !metadata.is_file() || path_has_link(root, path)? {
                return Ok(inspection.refuse("work envelope: new/opaque link or special file requires explicit review; preserve the work"));
            }
            let read_limit = crate::granularity::MAX_BINARY_CHANGE_BYTES
                .max(profile.max_changed_lines.saturating_mul(80) as u64);
            if metadata.len() > read_limit {
                return Ok(inspection.refuse(format!("work envelope: new file of {} bytes exceeds bounded inspection material; split or request explicit review", metadata.len())));
            }
            let bytes = bounded_file_bytes(&candidate, read_limit)?;
            match std::str::from_utf8(&bytes)
                .ok()
                .filter(|_| !bytes.contains(&0))
            {
                Some(body) => {
                    lines = lines.saturating_add(body.lines().count().max(body.len().div_ceil(80)));
                }
                None => {
                    inspection.binary_bytes =
                        inspection.binary_bytes.saturating_add(bytes.len() as u64);
                    if inspection.binary_bytes > crate::granularity::MAX_BINARY_CHANGE_BYTES {
                        let total = inspection.binary_bytes;
                        return Ok(inspection.refuse(binary_budget_refusal(total)));
                    }
                    inspection
                        .binary_changes
                        .push(localpilot_store::BinaryChange {
                            path: path.clone(),
                            before: None,
                            after: Some(binary_material(&bytes)),
                        });
                }
            }
            regions += 1;
        } else {
            for row in stat.lines() {
                let counts: Vec<_> = row.split('\t').take(2).map(str::parse::<usize>).collect();
                if let [Ok(added), Ok(removed)] = counts.as_slice() {
                    lines = lines.saturating_add(*added).saturating_add(*removed);
                } else {
                    return Ok(inspection.refuse(
                        "work envelope: unknown diff requires an explicit smaller reviewed unit",
                    ));
                }
            }
            // First reject a huge diff before materializing its patch.
            if lines <= profile.max_changed_lines {
                let diff = git(
                    root,
                    &[
                        "diff",
                        "--no-ext-diff",
                        "--no-textconv",
                        "--unified=0",
                        "--no-renames",
                        base,
                        "--",
                        path,
                    ],
                )?;
                let material = diff
                    .lines()
                    .filter(|line| {
                        (line.starts_with('+') && !line.starts_with("+++"))
                            || (line.starts_with('-') && !line.starts_with("---"))
                    })
                    .map(|line| line.len().saturating_sub(1).div_ceil(80))
                    .sum::<usize>();
                if material > profile.max_changed_lines {
                    return Ok(inspection.refuse("work envelope: changed byte material exceeds the profile (including giant single lines); preserve and split the work"));
                }
                regions += diff.lines().filter(|line| line.starts_with("@@ ")).count();
            }
        }
        if paths.len() > profile.max_files
            || lines > profile.max_changed_lines
            || regions > profile.max_regions
        {
            return Ok(inspection.refuse("work envelope: observed files, regions or changed lines exceed the effective profile; work is preserved, split/review it before committing"));
        }
    }
    Ok(inspection)
}

fn binary_budget_refusal(bytes: u64) -> String {
    format!("work envelope: binary change material of {bytes} bytes exceeds the unit's {} byte budget (old + new); preserve the work and split it or request a separate explicit review", crate::granularity::MAX_BINARY_CHANGE_BYTES)
}

fn binary_material(bytes: &[u8]) -> localpilot_store::BinaryMaterial {
    localpilot_store::BinaryMaterial {
        bytes: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(bytes)),
    }
}

fn bounded_file_bytes(path: &Path, limit: u64) -> Result<Vec<u8>, HarnessError> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(limit.saturating_add(1)).read_to_end(&mut bytes))
        .map_err(|source| HarnessError::Io {
            path: path.display().to_string(),
            source,
        })?;
    if bytes.len() as u64 > limit {
        return Err(HarnessError::Provider("work envelope: file grew beyond bounded inspection material; preserve and review the work".into()));
    }
    Ok(bytes)
}

enum MaterialSource {
    File(std::path::PathBuf),
    Blob {
        root: std::path::PathBuf,
        oid: String,
    },
}

struct SizedMaterial {
    bytes: u64,
    source: MaterialSource,
}

impl SizedMaterial {
    fn finish(self) -> Result<localpilot_store::BinaryMaterial, HarnessError> {
        match self.source {
            MaterialSource::File(path) => {
                let file = std::fs::File::open(&path).map_err(|source| HarnessError::Io {
                    path: path.display().to_string(),
                    source,
                })?;
                hash_bounded_reader(file, self.bytes)
            }
            MaterialSource::Blob { root, oid } => {
                // The immutable object's size was checked against the unit budget
                // before this child can return any payload bytes.
                let mut child = Command::new("git")
                    .args(["cat-file", "blob", &oid])
                    .current_dir(root)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .map_err(|e| HarnessError::Provider(format!("git cat-file: {e}")))?;
                let result = child
                    .stdout
                    .take()
                    .ok_or_else(|| HarnessError::Provider("baseline blob has no stdout".into()))
                    .and_then(|stdout| hash_bounded_reader(stdout, self.bytes));
                if result.is_err() {
                    let _ = child.kill();
                }
                let status = child
                    .wait()
                    .map_err(|e| HarnessError::Provider(format!("git cat-file: {e}")))?;
                if !status.success() {
                    return Err(HarnessError::Provider(
                        "cannot inspect bounded baseline blob".into(),
                    ));
                }
                result
            }
        }
    }
}

fn hash_bounded_reader(
    reader: impl Read,
    expected: u64,
) -> Result<localpilot_store::BinaryMaterial, HarnessError> {
    let mut reader = reader.take(expected.saturating_add(1));
    let mut hash = Sha256::new();
    let mut chunk = [0u8; 8192];
    let mut bytes = 0u64;
    loop {
        let n = reader
            .read(&mut chunk)
            .map_err(|e| HarnessError::Provider(format!("cannot hash bounded binary: {e}")))?;
        if n == 0 {
            break;
        }
        bytes = bytes.saturating_add(n as u64);
        if bytes > expected {
            return Err(HarnessError::Provider(
                "work envelope: binary grew beyond inspected size; preserve and review the work"
                    .into(),
            ));
        }
        hash.update(&chunk[..n]);
    }
    if bytes != expected {
        return Err(HarnessError::Provider(
            "work envelope: binary size changed during inspection; preserve and review the work"
                .into(),
        ));
    }
    Ok(localpilot_store::BinaryMaterial {
        bytes,
        sha256: format!("{:x}", hash.finalize()),
    })
}

fn baseline_blob(
    root: &Path,
    base: &str,
    path: &str,
) -> Result<Option<SizedMaterial>, HarnessError> {
    let spec = format!("{base}:{path}");
    let output = Command::new("git")
        .args(["rev-parse", "--verify", &spec])
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| HarnessError::Provider(format!("git rev-parse: {e}")))?;
    if !output.status.success() {
        return Ok(None);
    }
    let oid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let bytes = git(root, &["cat-file", "-s", &oid])?
        .trim()
        .parse::<u64>()
        .map_err(|e| HarnessError::Provider(format!("invalid baseline blob size: {e}")))?;
    Ok(Some(SizedMaterial {
        bytes,
        source: MaterialSource::Blob {
            root: root.to_path_buf(),
            oid,
        },
    }))
}

fn working_material(root: &Path, path: &str) -> Result<Option<SizedMaterial>, HarnessError> {
    let candidate = root.join(path);
    match std::fs::symlink_metadata(&candidate) {
        Ok(meta) if meta.is_file() && !path_has_link(root, path)? =>
            Ok(Some(SizedMaterial { bytes: meta.len(), source: MaterialSource::File(candidate) })),
        Ok(_) => Err(HarnessError::Provider("work envelope: binary link/special file requires explicit review; no linked content inspected".into())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(HarnessError::Io { path: path.to_string(), source }),
    }
}

fn path_has_link(root: &Path, path: &str) -> Result<bool, HarnessError> {
    let mut candidate = root.to_path_buf();
    for component in Path::new(path).components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Ok(true);
        }
        candidate.push(component);
        match std::fs::symlink_metadata(&candidate) {
            Ok(meta) => {
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if meta.file_attributes() & 0x400 != 0 {
                        return Ok(true);
                    }
                }
                if meta.file_type().is_symlink() {
                    return Ok(true);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(source) => {
                return Err(HarnessError::Io {
                    path: candidate.display().to_string(),
                    source,
                })
            }
        }
    }
    Ok(false)
}

/// The documents `intake` and `plan` write and nothing commits.
const PLAN_DOCUMENTS: [&str; 2] = ["brief.md", "PROGRESS.md"];

/// Commit `brief.md` and `PROGRESS.md` when they are all that is uncommitted.
///
/// A project that has just been through `intake` and `plan` has exactly these two
/// files uncommitted, and they are the harness's own: leaving them would make the
/// first `resume` stop on `no_stale_uncommitted` over files it wrote itself.
/// Ignoring them in that rule is not enough, because the same status feeds a
/// step's work-envelope check, where an untracked `brief.md` would count as an
/// extra file.
///
/// Anything else uncommitted means the working tree holds changes that are not
/// the harness's, and then nothing is committed here: the rule blocks as before,
/// and the user's changes are never swept into a harness commit. The commit holds
/// only these two paths, so it is its own commit and never part of a step's.
fn commit_plan_documents(root: &Path) -> Result<(), HarnessError> {
    let dirty = committable_status_paths(root)?;
    if dirty.is_empty()
        || !dirty
            .iter()
            .all(|path| PLAN_DOCUMENTS.contains(&path.as_str()))
    {
        return Ok(());
    }
    git_add_paths(root, &dirty)?;
    git(root, &["commit", "-m", "harness: add brief and plan"])?;
    Ok(())
}

fn has_unrelated_uncommitted_changes(root: &Path) -> Result<bool, HarnessError> {
    Ok(!committable_status_paths(root)?.is_empty())
}

fn committable_status_paths(root: &Path) -> Result<Vec<String>, HarnessError> {
    Ok(work_status_paths(root)?.0)
}

fn work_status_paths(root: &Path) -> Result<(Vec<String>, Vec<String>), HarnessError> {
    let status = git(
        root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    let mut generated = Vec::new();
    let mut material = Vec::new();
    for (state, path) in parse_status_entries(&status) {
        if is_runtime_state_path(&path) {
            continue;
        }
        if state == "??" && is_python_artifact(&path) {
            generated.push(path);
        } else {
            material.push(path);
        }
    }
    Ok((material, generated))
}

fn parse_status_entries(status: &str) -> Vec<(String, String)> {
    let mut records = status.split('\0');
    let mut paths = Vec::new();
    while let Some(record) = records.next() {
        if let Some(path) = record.get(3..).filter(|p| !p.is_empty()) {
            paths.push((record[..2].to_string(), path.to_string()));
            // Porcelain -z spells rename destination first, then source in a
            // separate record. Neither path is quoted or shell interpreted.
            if record
                .get(..2)
                .is_some_and(|state| state.contains(['R', 'C']))
            {
                records.next();
            }
        }
    }
    paths
}

fn is_python_artifact(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    normalized.ends_with(".pyc") || normalized.ends_with(".pyo")
}

fn is_runtime_state_path(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    normalized == ".localpilot"
        || normalized.starts_with(".localpilot/")
        || normalized == ".localmind"
        || normalized.starts_with(".localmind/")
}

fn first_blocking_reason(
    rule_engine: &RuleEngine,
    trigger: Trigger,
    ctx: &RuleContext,
) -> Option<String> {
    rule_engine
        .evaluate(trigger, ctx)
        .into_iter()
        .find_map(|(name, verdict)| match verdict {
            RuleVerdict::Block(reason) => Some(format!("{name}: {reason}")),
            _ => None,
        })
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkDiffBaseline {
    pub(crate) head: String,
    pub(crate) files: std::collections::BTreeMap<String, String>,
}

pub(crate) fn work_diff_baseline(root: &Path) -> Result<WorkDiffBaseline, HarnessError> {
    let head = git(root, &["rev-parse", "HEAD"])?;
    let files = committable_status_paths(root)?
        .into_iter()
        .map(|path| {
            let hash = work_fingerprint(root, &path)?;
            Ok((path, hash))
        })
        .collect::<Result<_, HarnessError>>()?;
    Ok(WorkDiffBaseline { head, files })
}

pub(crate) fn work_changed_paths(
    root: &Path,
    before: &WorkDiffBaseline,
) -> Result<Vec<String>, HarnessError> {
    let after = work_diff_baseline(root)?;
    let mut paths = before
        .files
        .keys()
        .chain(after.files.keys())
        .cloned()
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    paths.retain(|path| before.files.get(path) != after.files.get(path));
    Ok(paths)
}

fn work_fingerprint(root: &Path, path: &str) -> Result<String, HarnessError> {
    let candidate = root.join(path);
    match std::fs::symlink_metadata(&candidate) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::read_link(&candidate)
            .map(|p| format!("link:{}", p.display()))
            .map_err(|source| HarnessError::Io {
                path: path.to_string(),
                source,
            }),
        Ok(_) if path_has_link(root, path)? => Err(HarnessError::Provider(
            "cannot fingerprint a path through a link/junction; no linked content inspected".into(),
        )),
        Ok(_) => git(root, &["hash-object", "--", path]),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok("absent".to_string()),
        Err(source) => Err(HarnessError::Io {
            path: path.to_string(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::WORKER_PROMPT;

    #[test]
    fn worker_prompt_carries_the_doc_currency_cue() {
        // A step that changes observable behaviour must ship its docs in the same
        // plan; the worker prompt states that contract without forcing a second
        // file into a single-region unit.
        assert!(
            WORKER_PROMPT.contains("update the matching documentation in a bounded follow-up step"),
            "doc-currency cue missing from the worker prompt"
        );
    }
}

#[cfg(test)]
mod artifact_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::granularity::{ContextCapacity, ContextProvenance, Reliability, WorkProfile};

    fn repository() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]).unwrap();
        git(dir.path(), &["config", "user.name", "Fixture"]).unwrap();
        git(
            dir.path(),
            &["config", "user.email", "fixture@example.invalid"],
        )
        .unwrap();
        git(dir.path(), &["config", "core.autocrlf", "false"]).unwrap();
        git(dir.path(), &["commit", "--allow-empty", "-qm", "baseline"]).unwrap();
        dir
    }

    fn profile() -> WorkProfile {
        WorkProfile::resolve(
            ContextCapacity {
                used: 0,
                limit: 64_000,
                provenance: ContextProvenance::CallerSupplied,
            },
            Reliability::Strong,
            &localpilot_config::GranularityConfig::default(),
        )
    }

    fn write(root: &Path, path: &str, bytes: &[u8]) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn generated_patterns_are_narrow_and_separator_independent() {
        for path in [
            "__pycache__/x.cpython-314.pyc",
            "src\\__pycache__\\x.pyc",
            "x.pyc",
            "sub/x.pyo",
        ] {
            assert!(is_python_artifact(path), "{path}");
        }
        for path in [
            "__pycache__/real.txt",
            "src/__pycache__backup/x.txt",
            "real.py",
            "x.pyc.txt",
            ".pytest_cache/real.txt",
            "node_modules/x.js",
        ] {
            assert!(!is_python_artifact(path), "{path}");
        }
        assert_eq!(
            parse_status_entries("?? x.pyc\0A  staged.pyc\0R  dest\0source\0"),
            vec![
                ("??".into(), "x.pyc".into()),
                ("A ".into(), "staged.pyc".into()),
                ("R ".into(), "dest".into())
            ]
        );
    }

    #[test]
    fn only_untracked_artifacts_are_excluded_and_reported() {
        let dir = repository();
        write(dir.path(), ".gitignore", b"ignored/\n");
        git(dir.path(), &["add", ".gitignore"]).unwrap();
        git(dir.path(), &["commit", "-qm", "ignore"]).unwrap();
        let before = work_diff_baseline(dir.path()).unwrap();
        assert!(before.files.is_empty());
        write(dir.path(), "ignored/x.pyc", &[0, 255]);
        write(
            dir.path(),
            "__pycache__/deliverable.txt",
            b"real but artifact-shaped",
        );
        write(dir.path(), "x.pyc", &[0, 255]);
        assert!(work_diff_baseline(dir.path())
            .unwrap()
            .files
            .contains_key("__pycache__/deliverable.txt"));
        let inspection = inspect_work_diff(dir.path(), profile(), Some(&before), false).unwrap();
        assert_eq!(inspection.ignored_generated, vec!["x.pyc"]);
        assert!(inspection.binary_changes.is_empty());
        assert!(inspection.blocked.is_none());
        let mut no_material = profile();
        no_material.max_files = 0;
        assert!(
            inspect_work_diff(dir.path(), no_material, Some(&before), false)
                .unwrap()
                .blocked
                .is_some(),
            "a non-compiled deliverable inside a cache directory still counts"
        );
        // Git staging makes even a generated/ignored path meaningful.
        git(dir.path(), &["add", "-f", "ignored/x.pyc", "x.pyc"]).unwrap();
        let inspection = inspect_work_diff(dir.path(), profile(), Some(&before), false).unwrap();
        assert_eq!(inspection.binary_changes.len(), 2);
        assert_eq!(inspection.binary_bytes, 4);
        assert!(inspection.blocked.is_none());
        assert_eq!(
            work_changed_paths(dir.path(), &before).unwrap(),
            vec!["__pycache__/deliverable.txt", "ignored/x.pyc", "x.pyc"]
        );
    }

    #[test]
    fn tracked_ignored_binary_modification_delete_and_revert_count() {
        let dir = repository();
        write(dir.path(), ".gitignore", b"__pycache__/\n");
        write(dir.path(), "__pycache__/x.pyc", &[0, 255, 1]);
        git(
            dir.path(),
            &["add", "-f", ".gitignore", "__pycache__/x.pyc"],
        )
        .unwrap();
        git(dir.path(), &["commit", "-qm", "tracked artifact"]).unwrap();
        let baseline = work_diff_baseline(dir.path()).unwrap();
        write(dir.path(), "__pycache__/x.pyc", &[0, 255, 2, 3]);
        let inspection = inspect_work_diff(dir.path(), profile(), Some(&baseline), false).unwrap();
        assert!(inspection.blocked.is_none());
        assert_eq!(inspection.binary_bytes, 7);
        let change = &inspection.binary_changes[0];
        assert_eq!(change.before, Some(binary_material(&[0, 255, 1])));
        assert_eq!(change.after, Some(binary_material(&[0, 255, 2, 3])));
        let mut readonly = profile();
        readonly.max_regions = 0;
        assert!(
            inspect_work_diff(dir.path(), readonly, Some(&baseline), false)
                .unwrap()
                .blocked
                .is_some()
        );
        write(dir.path(), "__pycache__/x.pyc", &[0, 255, 1]);
        assert!(
            inspect_work_diff(dir.path(), profile(), Some(&baseline), false)
                .unwrap()
                .binary_changes
                .is_empty()
        );
        std::fs::remove_file(dir.path().join("__pycache__/x.pyc")).unwrap();
        let inspection = inspect_work_diff(dir.path(), profile(), Some(&baseline), false).unwrap();
        assert_eq!(inspection.binary_bytes, 3);
        assert_eq!(inspection.binary_changes[0].after, None);
    }

    #[test]
    fn exact_binary_budget_is_accepted_and_old_plus_new_overflow_refused() {
        let dir = repository();
        write(dir.path(), "asset.bin", &vec![0; 32 * 1024]);
        git(dir.path(), &["add", "asset.bin"]).unwrap();
        git(dir.path(), &["commit", "-qm", "asset"]).unwrap();
        let baseline = work_diff_baseline(dir.path()).unwrap();
        let mut bytes = vec![0; 32 * 1024];
        bytes[0] = 1;
        write(dir.path(), "asset.bin", &bytes);
        let inspection = inspect_work_diff(dir.path(), profile(), Some(&baseline), false).unwrap();
        assert_eq!(
            inspection.binary_bytes,
            crate::granularity::MAX_BINARY_CHANGE_BYTES
        );
        assert!(inspection.blocked.is_none());
        bytes.push(0);
        write(dir.path(), "asset.bin", &bytes);
        let inspection = inspect_work_diff(dir.path(), profile(), Some(&baseline), false).unwrap();
        assert!(inspection.blocked.unwrap().contains("65537 bytes"));
        assert!(
            inspection.binary_changes.is_empty(),
            "refuse before reading either payload"
        );
    }

    #[test]
    fn aggregate_binary_material_and_file_regions_are_not_zero_cost() {
        let dir = repository();
        let baseline = work_diff_baseline(dir.path()).unwrap();
        write(dir.path(), "a.bin", &vec![0; 40 * 1024]);
        write(dir.path(), "b.bin", &vec![0; 30 * 1024]);
        let inspection = inspect_work_diff(dir.path(), profile(), Some(&baseline), false).unwrap();
        assert!(inspection.blocked.unwrap().contains("71680 bytes"));
        std::fs::remove_file(dir.path().join("b.bin")).unwrap();
        let mut no_files = profile();
        no_files.max_files = 0;
        assert!(
            inspect_work_diff(dir.path(), no_files, Some(&baseline), false)
                .unwrap()
                .blocked
                .is_some()
        );
    }

    #[test]
    fn untouched_initial_dirty_binary_is_excluded_but_a_new_change_is_inspected() {
        let dir = repository();
        write(dir.path(), "asset.bin", &[0, 1]);
        git(dir.path(), &["add", "asset.bin"]).unwrap();
        git(dir.path(), &["commit", "-qm", "asset"]).unwrap();
        write(dir.path(), "asset.bin", &[0, 2]);
        let baseline = work_diff_baseline(dir.path()).unwrap();
        assert!(
            inspect_work_diff(dir.path(), profile(), Some(&baseline), false)
                .unwrap()
                .binary_changes
                .is_empty()
        );
        write(dir.path(), "asset.bin", &[0, 3]);
        assert_eq!(
            inspect_work_diff(dir.path(), profile(), Some(&baseline), false)
                .unwrap()
                .binary_changes
                .len(),
            1
        );
    }

    #[test]
    fn binary_rename_counts_the_deleted_source_and_added_destination() {
        let dir = repository();
        write(dir.path(), "old.bin", &[0, 255]);
        git(dir.path(), &["add", "old.bin"]).unwrap();
        git(dir.path(), &["commit", "-qm", "asset"]).unwrap();
        let baseline = work_diff_baseline(dir.path()).unwrap();
        git(dir.path(), &["mv", "old.bin", "new.bin"]).unwrap();
        let inspection = inspect_work_diff(dir.path(), profile(), Some(&baseline), false).unwrap();
        assert!(inspection.blocked.is_none());
        assert_eq!(inspection.binary_bytes, 4);
        assert_eq!(inspection.binary_changes.len(), 2);
        assert!(inspection
            .binary_changes
            .iter()
            .any(|c| c.path == "old.bin" && c.after.is_none()));
        assert!(inspection
            .binary_changes
            .iter()
            .any(|c| c.path == "new.bin" && c.before.is_none()));
        let mut one_file = profile();
        one_file.max_files = 1;
        assert!(
            inspect_work_diff(dir.path(), one_file, Some(&baseline), false)
                .unwrap()
                .blocked
                .is_some()
        );
    }

    #[test]
    fn linked_directories_are_not_fingerprinted_or_read_as_material() {
        let dir = repository();
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "external.bin", &[0, 255, 7]);
        let link = dir.path().join("linked");
        #[cfg(windows)]
        {
            // Junction creation needs no developer-mode symlink privilege.
            let script = format!(
                "New-Item -ItemType Junction -Path '{}' -Target '{}' | Out-Null",
                link.display().to_string().replace('\'', "''"),
                outside.path().display().to_string().replace('\'', "''"),
            );
            let output = Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        assert!(path_has_link(dir.path(), "linked/external.bin").unwrap());
        assert!(work_fingerprint(dir.path(), "linked/external.bin").is_err());
        assert!(working_material(dir.path(), "linked/external.bin").is_err());
        assert_eq!(
            std::fs::read(outside.path().join("external.bin")).unwrap(),
            [0, 255, 7]
        );
    }

    #[test]
    fn discard_preserves_excluded_artifacts_and_execution_evidence() {
        let dir = repository();
        write(dir.path(), "source.txt", b"committed");
        git(dir.path(), &["add", "source.txt"]).unwrap();
        git(dir.path(), &["commit", "-qm", "source"]).unwrap();
        write(dir.path(), "existing.pyc", &[0, 255, 1]);
        write(dir.path(), "source.txt", b"failed attempt");
        write(dir.path(), "new[1].txt", b"failed attempt");
        write(dir.path(), "__pycache__/new.pyc", &[0, 255, 2]);
        write(
            dir.path(),
            ".localpilot/events.jsonl",
            b"retained audit fixture",
        );
        let material_names: Vec<String> = (0..400)
            .map(|i| format!("large-{i:04}-{}.txt", "x".repeat(100)))
            .collect();
        assert!(material_names.iter().map(String::len).sum::<usize>() > 32768);
        for name in &material_names {
            write(dir.path(), name, b"discarded oversized attempt");
        }
        // No ignore rules: the explicit material-path policy must protect them.
        restore_committed_state(dir.path()).unwrap();
        assert!(material_names
            .iter()
            .all(|name| !dir.path().join(name).exists()));
        assert_eq!(
            std::fs::read(dir.path().join("source.txt")).unwrap(),
            b"committed"
        );
        assert!(!dir.path().join("new[1].txt").exists());
        assert_eq!(
            std::fs::read(dir.path().join("existing.pyc")).unwrap(),
            [0, 255, 1]
        );
        assert_eq!(
            std::fs::read(dir.path().join("__pycache__/new.pyc")).unwrap(),
            [0, 255, 2]
        );
        assert_eq!(
            std::fs::read(dir.path().join(".localpilot/events.jsonl")).unwrap(),
            b"retained audit fixture"
        );
    }

    #[test]
    fn literal_git_names_preserve_binary_and_text_accounting() {
        let dir = repository();
        write(dir.path(), "asset[1].bin", &[0, 1]);
        write(dir.path(), "asset1.bin", &[0, 9]);
        git(dir.path(), &["add", "-A"]).unwrap();
        git(dir.path(), &["commit", "-qm", "literal assets"]).unwrap();
        let baseline = work_diff_baseline(dir.path()).unwrap();
        write(dir.path(), "asset[1].bin", &[0, 2]);
        let inspection = inspect_work_diff(dir.path(), profile(), Some(&baseline), false).unwrap();
        assert!(inspection.blocked.is_none());
        assert_eq!(inspection.binary_bytes, 4);
        assert_eq!(inspection.binary_changes.len(), 1);
        let change = &inspection.binary_changes[0];
        assert_eq!(change.path, "asset[1].bin");
        assert_eq!(change.before, Some(binary_material(&[0, 1])));
        assert_eq!(change.after, Some(binary_material(&[0, 2])));
        write(dir.path(), "asset[1].bin", &vec![0; 64 * 1024 - 1]);
        let inspection = inspect_work_diff(dir.path(), profile(), Some(&baseline), false).unwrap();
        assert!(
            inspection.blocked.unwrap().contains("65537 bytes"),
            "literal names must not bypass old-plus-new byte accounting"
        );

        let text_dir = repository();
        write(text_dir.path(), "text[1].txt", b"before\n");
        write(text_dir.path(), "text1.txt", &[0, 1]);
        git(text_dir.path(), &["add", "-A"]).unwrap();
        git(text_dir.path(), &["commit", "-qm", "mixed literal names"]).unwrap();
        let baseline = work_diff_baseline(text_dir.path()).unwrap();
        write(
            text_dir.path(),
            "text[1].txt",
            "line\n".repeat(300).as_bytes(),
        );
        write(text_dir.path(), "text1.txt", &[0, 2]);
        let inspection =
            inspect_work_diff(text_dir.path(), profile(), Some(&baseline), false).unwrap();
        assert!(
            inspection.blocked.unwrap().contains("changed lines"),
            "a matching binary must not turn a text diff into binary accounting"
        );
    }
}
