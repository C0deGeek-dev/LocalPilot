//! `localpilot eval` — run the agent headless on one problem and emit the
//! machine-readable capability scorecard (JSON) to stdout.
//!
//! This is the solver entry point an external benchmark runner drives: it runs
//! the same harness a real session uses, captures the produced diff and the
//! session event trace, and assembles the scorecard. The `results` block is
//! graded by an optional `--test` command; an external grader (e.g. a benchmark's
//! own container) may instead fill `results` after the fact. Only the scorecard
//! JSON goes to stdout — model output is suppressed — so the line is pipe-safe.

use std::path::Path;
use std::process::Command;

use anyhow::Context;
use localpilot_harness::{build_scorecard, DiffStat, ResultsBlock, RunInputs};
use localpilot_sandbox::Profile;
use localpilot_store::Store;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Options for one eval run.
pub struct EvalOptions<'a> {
    pub problem: &'a str,
    pub model: &'a str,
    pub provider_id: Option<&'a str>,
    pub profile: Profile,
    pub arm: &'a str,
    pub task: &'a str,
    /// A grading command; passed (exit 0) sets `results.passed`. When absent the
    /// run is emitted ungraded (an external grader fills `results`).
    pub test_command: Option<&'a str>,
    /// Path to a gold unified diff, for the vs-gold ratio.
    pub gold_diff: Option<&'a Path>,
    /// Enable the verify-before-done gate for this run, overriding config. The
    /// benchmark's full+verify arm sets this so a turn re-runs a build/test
    /// verification before finalizing.
    pub verify: bool,
    /// Verification command override for the gate (a single command line). When
    /// `None` the gate detects a command from the workspace stack.
    pub verify_command: Option<&'a str>,
    /// Learn from this run: after the turn, close the session out into LocalMind
    /// so it yields review-gated lesson candidates (scope-routed to the
    /// machine-wide store). Off by default, so a plain `eval` neither reads nor
    /// writes accumulated memory and stays a clean-room capability measurement.
    pub learn: bool,
}

/// Run one eval and print the capability scorecard JSON to stdout.
///
/// # Errors
/// Returns an error if the workspace is not a git repository, the runtime cannot
/// be built, or git/IO fails.
pub async fn run_eval(opts: EvalOptions<'_>) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let base = git_head(&cwd)
        .context("`eval` needs a git repository in the working directory to capture the diff")?;

    let mut runtime =
        crate::session_cmd::build_runtime(&cwd, opts.model, opts.provider_id, opts.profile, true)
            .await?;
    // `--verify` opts this run into the verify-before-done gate regardless of
    // config, so a benchmark arm can enable it without writing a config file.
    if opts.verify {
        runtime.set_verify_before_done(true, opts.verify_command.map(str::to_string));
    }
    let session = runtime.session_id();

    // Run the turn; model text is discarded (stdout is reserved for the JSON).
    let (events_tx, _rx) = broadcast::channel(1024);
    let cancel = CancellationToken::new();
    let started = std::time::Instant::now();
    let _reason = runtime.run_turn(opts.problem, &events_tx, &cancel).await;
    let wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    // Capture the produced change as a unified diff (excluding harness bookkeeping).
    git(&cwd, &["add", "-A"])?;
    let diff_text = git(
        &cwd,
        &[
            "diff",
            "--no-color",
            "--cached",
            &base,
            "--",
            ".",
            ":!PROGRESS.md",
            ":!DECISIONS.md",
        ],
    )?;

    let results = grade(opts.test_command, &cwd)?;
    let gold = match opts.gold_diff {
        Some(path) => Some(DiffStat::from_unified(
            &std::fs::read_to_string(path)
                .with_context(|| format!("reading gold diff {}", path.display()))?,
        )),
        None => None,
    };
    let events = Store::open(&cwd).read_events(session)?;

    // Light up the validity metric: validate each call's arguments against the
    // builtin tool schemas so the scorecard reports `schema_valid_rate`. MCP
    // tools have no typed schema and stay unvalidated (`None`).
    let registry = localpilot_tools::ToolRegistry::with_builtins();
    let schema_validator = |name: &str, input: &serde_json::Value| -> Option<bool> {
        registry
            .get(name)
            .map(|tool| localpilot_tools::is_input_valid(&tool.schema(), input))
    };

    let card = build_scorecard(RunInputs {
        task: opts.task.to_string(),
        arm: opts.arm.to_string(),
        model: opts.model.to_string(),
        results,
        diff_text: &diff_text,
        gold,
        gate: &[],
        events: &events,
        wall_ms,
        schema_validator: Some(&schema_validator),
    });
    println!("{}", card.to_json()?);

    // Learn from this run when asked: close the session out into LocalMind so it
    // yields review-gated lesson candidates (good and anti-pattern, scope-routed
    // to the machine-wide store per the classifier). Runs *after* the diff capture
    // and the scorecard print, so it never pollutes the captured diff or the
    // JSON-only stdout (close_out reports to stderr); a no-op when learning is
    // disabled or the session is empty.
    if opts.learn {
        crate::context_inject::close_out(&cwd, runtime.session_id());
    }
    Ok(())
}

/// Grade the run with the optional `--test` command (exit 0 = passed). With no
/// command the run is emitted ungraded, for an external grader to fill in.
fn grade(test_command: Option<&str>, cwd: &Path) -> anyhow::Result<ResultsBlock> {
    let Some(command) = test_command else {
        return Ok(ResultsBlock {
            passed: false,
            regression_safe: true,
            partial_credit: 0.0,
            tests_total: 0,
            tests_passed: 0,
        });
    };
    let passed = run_shell(command, cwd)?;
    Ok(ResultsBlock {
        passed,
        regression_safe: passed,
        partial_credit: if passed { 1.0 } else { 0.0 },
        tests_total: 1,
        tests_passed: u32::from(passed),
    })
}

/// Run a grading command through the platform shell; returns whether it exited 0.
fn run_shell(command: &str, cwd: &Path) -> anyhow::Result<bool> {
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.args(["/C", command]);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = Command::new("sh");
        c.args(["-c", command]);
        c
    };
    let status = cmd
        .current_dir(cwd)
        .status()
        .with_context(|| format!("running grading command: {command}"))?;
    Ok(status.success())
}

/// `git rev-parse HEAD`, or an error when `cwd` is not a git repository.
fn git_head(cwd: &Path) -> anyhow::Result<String> {
    Ok(git(cwd, &["rev-parse", "HEAD"])?.trim().to_string())
}

/// Run a git command in `cwd` and return its stdout.
fn git(cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("running git {args:?}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
