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
use localpilot_selfreview::{
    review, ProcessFriction, Report, ReviewOptions, FRICTION_AUDIT_PROMPT,
};

/// Options for one self-review run.
pub struct SelfReviewArgs<'a> {
    /// Emit the machine-readable JSON report instead of the human summary.
    pub json: bool,
    /// Include the heuristic, low-confidence missing-test detector.
    pub missing_tests: bool,
    /// Include the whole-repo teardown-sweep detectors (the cleanup-audit
    /// categories). Off by default; this is the on-demand path to the same sweep
    /// the harness runs at completion when `[harness] teardown_sweep` is on.
    pub cleanup: bool,
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
            include_cleanup: args.cleanup,
        },
    );
    if args.json {
        writeln!(out, "{}", report.to_json()?)?;
    } else {
        write!(out, "{}", report.human_summary())?;
    }
    Ok(())
}

/// Run the whole-repo teardown sweep: a read-only self-review with the
/// cleanup-audit detectors on. Strictly read-only — it scans and returns a report,
/// touching nothing. It deliberately skips the prior-lesson fetch (which would
/// initialise the LocalMind store on a repo that has none) so the advisory
/// completion sweep leaves a finished run's outputs byte-for-byte untouched. The
/// on-demand `self-review --cleanup` path still folds prior lessons in via [`run`].
#[must_use]
pub fn teardown_review(root: &Path) -> Report {
    review(
        root,
        &ReviewOptions {
            include_cleanup: true,
            ..ReviewOptions::default()
        },
    )
}

/// The advisory completion teardown sweep wired into the harness completion seam.
///
/// When `enabled`, it runs the read-only [`teardown_review`] over `root` and prints
/// a ranked advisory summary; when not, it does nothing. It never blocks
/// completion, edits code, or commits — the only effect is the printed summary, so
/// a finished run's outputs are untouched whether the sweep is on or off. Returns
/// whether the sweep ran.
///
/// # Errors
/// Returns an error only if writing the summary to `out` fails; the caller (a
/// finished run) ignores it so a write hiccup cannot break completion.
pub fn run_completion_sweep(
    root: &Path,
    enabled: bool,
    out: &mut dyn Write,
) -> anyhow::Result<bool> {
    if !enabled {
        return Ok(false);
    }
    let report = teardown_review(root);
    writeln!(
        out,
        "teardown sweep: {} advisory cleanup finding(s) across {} file(s)",
        report
            .findings
            .iter()
            .filter(|f| f.kind.is_cleanup())
            .count(),
        report.scanned_files
    )?;
    write!(out, "{}", report.human_summary())?;
    Ok(true)
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;

    /// Every file under `root` as path → byte-length, for the read-only assertion.
    fn snapshot(root: &Path) -> BTreeMap<PathBuf, u64> {
        let mut map = BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(meta) = entry.metadata() {
                    map.insert(path, meta.len());
                }
            }
        }
        map
    }

    /// The completion sweep runs only when the harness flag is on; off, it is a
    /// no-op that prints nothing.
    #[test]
    fn completion_sweep_runs_only_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        fs::write(root.join("keep.rs.bak"), "pub fn gone() {}\n").unwrap();

        let mut off = Vec::new();
        assert!(!run_completion_sweep(root, false, &mut off).unwrap());
        assert!(off.is_empty(), "disabled sweep prints nothing");

        let mut on = Vec::new();
        assert!(run_completion_sweep(root, true, &mut on).unwrap());
        let text = String::from_utf8(on).unwrap();
        assert!(text.contains("teardown sweep"), "{text}");
    }

    /// A completion sweep leaves the finished run's files byte-for-byte untouched.
    #[test]
    fn completion_sweep_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        fs::write(root.join("a.rs"), "#[allow(dead_code)]\npub fn f() {}\n").unwrap();

        let before = snapshot(root);
        let mut out = Vec::new();
        run_completion_sweep(root, true, &mut out).unwrap();
        let after = snapshot(root);
        assert_eq!(before, after, "the completion sweep must be read-only");
    }
}
