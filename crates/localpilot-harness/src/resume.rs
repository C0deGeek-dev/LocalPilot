//! `harness resume`: run the next plan step end to end — work the step through
//! the session loop, run configured tests, evaluate the completion rules, then
//! commit the step and the progress update.

use std::path::Path;
use std::process::Command;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use localpilot_config::{AutoFix, Cadence, CheckConfig};
use localpilot_core::StructuredSummary;
use localpilot_llm::QuotaInfo;
use localpilot_quota::{estimate_window, PausedRun};
use localpilot_store::SessionEventKind;

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
documentation in a bounded follow-up step in this plan before completing the feature. Do not start any other step.\n\nStep: ";

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
            if let Some(reason) = work_diff_block(root, profile, work_baseline.as_ref(), true)? {
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
        if let Some(reason) = work_diff_block(root, profile, work_baseline.as_ref(), true)? {
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
/// tracked files reset to `HEAD`, files the attempt created removed. Ignored
/// files (the `.localpilot/` execution record) are untouched (`clean`
/// without `-x`), and a resume refuses to start over unrelated uncommitted
/// changes, so everything removed here belongs to the discarded attempt.
fn restore_committed_state(root: &Path) -> Result<(), HarnessError> {
    git(root, &["reset", "--hard", "HEAD"])?;
    git(root, &["clean", "-fd"])?;
    Ok(())
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

fn git_add_paths(root: &Path, paths: &[String]) -> Result<(), HarnessError> {
    let mut args = vec!["add", "--"];
    args.extend(paths.iter().map(String::as_str));
    git(root, &args).map(|_| ())
}

pub(crate) fn work_diff_block(
    root: &Path,
    profile: crate::granularity::WorkProfile,
    baseline: Option<&WorkDiffBaseline>,
    ignore_progress: bool,
) -> Result<Option<String>, HarnessError> {
    let base = baseline.map_or("HEAD", |before| before.head.trim());
    let mut paths = committable_status_paths(root)?;
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
    paths.retain(|p| {
        !is_runtime_state_path(p)
            && (!ignore_progress || p != "PROGRESS.md")
            && baseline
                .is_none_or(|before| before.files.get(p) != work_fingerprint(root, p).ok().as_ref())
    });
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
        if stat.is_empty() {
            // New files have no tracked diff. Bound bytes before reading and
            // reject opaque/binary material rather than treating it as zero.
            let candidate = root.join(path);
            let metadata =
                std::fs::symlink_metadata(&candidate).map_err(|source| HarnessError::Io {
                    path: path.clone(),
                    source,
                })?;
            if !metadata.is_file()
                || metadata.len() > profile.max_changed_lines.saturating_mul(80) as u64
            {
                return Ok(Some("work envelope: new/opaque file exceeds bounded material; preserve the work and split the step before committing".to_string()));
            }
            let body = std::fs::read_to_string(&candidate).map_err(|source| HarnessError::Io {
                path: path.clone(),
                source,
            })?;
            lines = lines.saturating_add(body.lines().count().max(body.len().div_ceil(80)));
            regions += 1;
        } else {
            for row in stat.lines() {
                let counts: Vec<_> = row.split('\t').take(2).map(str::parse::<usize>).collect();
                if let [Ok(added), Ok(removed)] = counts.as_slice() {
                    lines = lines.saturating_add(*added).saturating_add(*removed);
                } else {
                    return Ok(Some("work envelope: binary/unknown diff requires an explicit smaller reviewed unit".to_string()));
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
                    return Ok(Some("work envelope: changed byte material exceeds the profile (including giant single lines); preserve and split the work".to_string()));
                }
                regions += diff.lines().filter(|line| line.starts_with("@@ ")).count();
            }
        }
        if paths.len() > profile.max_files
            || lines > profile.max_changed_lines
            || regions > profile.max_regions
        {
            return Ok(Some("work envelope: observed files, regions or changed lines exceed the effective profile; work is preserved, split/review it before committing".to_string()));
        }
    }
    Ok(None)
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
    let status = git(
        root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    Ok(parse_status_paths(&status)
        .into_iter()
        .filter(|path| !is_runtime_state_path(path))
        .collect())
}

fn parse_status_paths(status: &str) -> Vec<String> {
    let mut records = status.split('\0');
    let mut paths = Vec::new();
    while let Some(record) = records.next() {
        if let Some(path) = record.get(3..).filter(|p| !p.is_empty()) {
            paths.push(path.to_string());
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
