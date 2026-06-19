//! `localpilot self-review`: the read-only repo-health findings surface.
//!
//! Runs the read-only self-review scan over the workspace, folds in any prior
//! accepted lessons as context (so findings are informed by past outcomes), and
//! optionally merges a model-emitted harness-friction block. It writes nothing —
//! the output is an advisory ranked report (human summary or JSON). This is the
//! read-only front of the human-gated self-improvement loop (ADR-0034).

use std::io::Write;
use std::path::Path;

use anyhow::Context;
use localpilot_selfreview::{review, ProcessFriction, ReviewOptions, FRICTION_AUDIT_PROMPT};

/// Options for one self-review run.
pub struct SelfReviewArgs<'a> {
    /// Emit the machine-readable JSON report instead of the human summary.
    pub json: bool,
    /// Include the heuristic, low-confidence missing-test detector.
    pub missing_tests: bool,
    /// A file holding a model's friction-findings block to fold in.
    pub friction_file: Option<&'a Path>,
    /// A file holding a captured run's capability scorecard JSON; its `process`
    /// block is folded in as measured (auto-captured) friction.
    pub process_file: Option<&'a Path>,
}

/// Run the read-only self-review and print the report.
///
/// # Errors
/// Returns an error only if a named friction file cannot be read or output
/// cannot be written; the scan itself is best-effort and never fails the command.
pub fn run(root: &Path, args: &SelfReviewArgs, out: &mut dyn Write) -> anyhow::Result<()> {
    let friction_block = match args.friction_file {
        Some(path) => Some(
            std::fs::read_to_string(path)
                .with_context(|| format!("reading friction file {}", path.display()))?,
        ),
        None => None,
    };
    let process = match args.process_file {
        Some(path) => Some(process_friction_from_scorecard(path)?),
        None => None,
    };
    let report = review(
        root,
        &ReviewOptions {
            prior_lessons: prior_lessons(root),
            friction_block,
            process,
            include_missing_tests: args.missing_tests,
        },
    );
    if args.json {
        writeln!(out, "{}", report.to_json()?)?;
    } else {
        write!(out, "{}", report.human_summary())?;
    }
    Ok(())
}

/// Read a capability scorecard JSON file and project its `process` block into the
/// measured-friction input. The scorecard is the harness's own emitted artefact;
/// only its `process` object is read, so the self-review crate stays decoupled
/// from the harness scorecard type.
///
/// # Errors
/// Returns an error if the file cannot be read, is not valid JSON, or carries no
/// `process` object.
fn process_friction_from_scorecard(path: &Path) -> anyhow::Result<ProcessFriction> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading scorecard file {}", path.display()))?;
    let scorecard: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing scorecard JSON {}", path.display()))?;
    let process = scorecard
        .get("process")
        .with_context(|| format!("scorecard {} has no `process` block", path.display()))?;
    serde_json::from_value(process.clone())
        .with_context(|| format!("reading the `process` block of {}", path.display()))
}

/// Print the friction audit prompt a host runs to elicit a friction block (which
/// can then be fed back through `--friction-file`).
///
/// # Errors
/// Returns an error only if output cannot be written.
pub fn print_audit_prompt(out: &mut dyn Write) -> anyhow::Result<()> {
    writeln!(out, "{FRICTION_AUDIT_PROMPT}")?;
    Ok(())
}

/// Best-effort prior-lesson retrieval: accepted memories become advisory
/// context for the scan. A project with no learning store yields none, and a
/// read error is swallowed — prior lessons only inform findings, they never gate
/// the scan.
fn prior_lessons(root: &Path) -> Vec<String> {
    localpilot_localmind::memory_list(root)
        .map(|memories| {
            memories
                .into_iter()
                .map(|memory| format!("{} — {}", memory.path, memory.body))
                .collect()
        })
        .unwrap_or_default()
}
