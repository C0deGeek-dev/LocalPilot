//! Read-only self-review: scan a repository for advisory health findings.
//!
//! `localpilot-selfreview` is the front of the human-gated self-improvement loop
//! (ADR-0034): the **read-only** `observe → detect → propose` stages. It scans a
//! repo for drift, leftover markers, stale decision indexes, incomplete plan
//! rows, broken doc links, and (heuristically) missing tests; it folds in
//! model-emitted harness-friction findings; and it ranks everything into one
//! advisory [`Report`]. It writes nothing — every output is data the reader acts
//! on, never an action. The patch-generating, human-gated half of the loop lives
//! elsewhere.
//!
//! Prior lessons (retrieved by the host from the learning engine) are *injected*
//! rather than fetched here, so this crate stays free of a memory dependency and
//! fully offline-testable.
#![forbid(unsafe_code)]

mod detectors;
mod finding;
mod friction;
mod process_friction;

pub use finding::{Finding, FindingKind, Report, Severity, Span, REPORT_SCHEMA};
pub use friction::{parse_friction_findings, FRICTION_AUDIT_PROMPT};
pub use process_friction::{process_friction_findings, ProcessFriction};

use std::path::Path;

/// Inputs to a self-review beyond the repo path.
#[derive(Debug, Default, Clone)]
pub struct ReviewOptions {
    /// Relevant prior lessons/conventions the host retrieved from the learning
    /// engine at scan start (read-only consumption). A lesson that names a
    /// finding's file marks it as a recurring issue.
    pub prior_lessons: Vec<String>,
    /// A raw model audit block (the friction-findings source). `None` skips it.
    pub friction_block: Option<String>,
    /// Measured process signals from a captured run's scorecard `process` block
    /// (the deterministic, auto-captured friction source). `None` skips it.
    pub process: Option<ProcessFriction>,
    /// Include the heuristic, low-confidence missing-test detector. Off by
    /// default because it cannot see sibling test crates and is the noisiest
    /// signal; the host opts in.
    pub include_missing_tests: bool,
}

/// Run a read-only self-review of `root` and return a ranked, advisory report.
/// Performs no writes.
#[must_use]
pub fn review(root: &Path, options: &ReviewOptions) -> Report {
    let (mut findings, scanned) = detectors::scan(root, options.include_missing_tests);
    if let Some(block) = &options.friction_block {
        findings.extend(parse_friction_findings(block));
    }
    if let Some(process) = &options.process {
        findings.extend(process_friction_findings(process));
    }
    apply_prior_lessons(&mut findings, &options.prior_lessons);
    Report::ranked(findings, scanned)
}

/// Let prior lessons inform the findings: a lesson that names a finding's file is
/// a recurring-issue signal, so the finding's confidence is nudged up (capped)
/// and annotated. Deterministic and conservative — it never invents or removes a
/// finding, only reflects that a prior outcome touched the same file.
fn apply_prior_lessons(findings: &mut [Finding], lessons: &[String]) {
    if lessons.is_empty() {
        return;
    }
    let lowered: Vec<String> = lessons.iter().map(|l| l.to_ascii_lowercase()).collect();
    for finding in findings.iter_mut() {
        let Some(path) = finding.path.as_ref() else {
            continue;
        };
        let path_lower = path.to_ascii_lowercase();
        if path_lower.len() >= 3 && lowered.iter().any(|lesson| lesson.contains(&path_lower)) {
            finding.confidence = (finding.confidence + 0.1).min(1.0);
            finding.evidence.push_str(" (recurs in a prior lesson)");
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    /// A snapshot of every file under `root` as path → byte-length, for the
    /// "review writes nothing" assertion.
    fn snapshot(root: &Path) -> BTreeMap<PathBuf, u64> {
        let mut map = BTreeMap::new();
        for entry in ignore::WalkBuilder::new(root)
            .hidden(false)
            .build()
            .flatten()
        {
            if entry.file_type().is_some_and(|t| t.is_file()) {
                let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
                map.insert(entry.path().to_path_buf(), len);
            }
        }
        map
    }

    /// Golden fixture: a small repo with one of each detectable problem must
    /// produce the expected kinds, and the report must be deterministically
    /// ranked (high severity × confidence first).
    #[test]
    fn golden_repo_yields_expected_ranked_findings() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "src/worker.rs",
            "pub fn run() {}\n// TODO: handle retries\n",
        );
        write(
            root,
            "docs/guide.md",
            "see [missing](./nope.md) for details\n",
        );
        write(
            root,
            "docs/decisions.md",
            "## ADR-0007\n## ADR-0008\nlatest is ADR-0008\n",
        );
        write(
            root,
            "REGISTRY.md",
            "index of decisions: latest ADR-0007 (7 ADRs)\n",
        );
        write(root, "plans/p.md", "| box | status |\n| 1 | TODO |\n");

        let report = review(root, &ReviewOptions::default());
        let kinds: Vec<FindingKind> = report.findings.iter().map(|f| f.kind).collect();
        assert!(kinds.contains(&FindingKind::Todo), "{kinds:?}");
        assert!(kinds.contains(&FindingKind::DocDrift), "{kinds:?}");
        assert!(kinds.contains(&FindingKind::StaleAdr), "{kinds:?}");
        assert!(kinds.contains(&FindingKind::BrokenPlan), "{kinds:?}");

        // Ranked: scores are non-increasing.
        let scores: Vec<f32> = report.findings.iter().map(|f| f.score()).collect();
        for pair in scores.windows(2) {
            assert!(pair[0] >= pair[1], "report must be ranked: {scores:?}");
        }
        // Stable schema tag for consumers.
        assert_eq!(report.schema, REPORT_SCHEMA);
    }

    /// The scan must not write, create, or delete anything in the repo.
    #[test]
    fn review_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/a.rs", "pub fn f() {}\n// FIXME: later\n");
        write(root, "README.md", "[x](./gone.md)\n");

        let before = snapshot(root);
        let _ = review(
            root,
            &ReviewOptions {
                include_missing_tests: true,
                ..ReviewOptions::default()
            },
        );
        let after = snapshot(root);
        assert_eq!(before, after, "review must be read-only");
    }

    /// False-positive guardrail: a tested file is not flagged missing-test, and a
    /// valid doc link is not flagged as drift.
    #[test]
    fn no_false_positives_for_tested_file_and_valid_link() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "src/ok.rs",
            "pub fn f() {}\n#[cfg(test)]\nmod tests { #[test] fn t() {} }\n",
        );
        write(root, "docs/a.md", "see [b](./b.md)\n");
        write(root, "docs/b.md", "hello\n");

        let report = review(
            root,
            &ReviewOptions {
                include_missing_tests: true,
                ..ReviewOptions::default()
            },
        );
        assert!(
            !report
                .findings
                .iter()
                .any(|f| f.kind == FindingKind::MissingTest),
            "a tested file must not be flagged: {:?}",
            report.findings
        );
        assert!(
            !report
                .findings
                .iter()
                .any(|f| f.kind == FindingKind::DocDrift),
            "a valid link must not be flagged: {:?}",
            report.findings
        );
    }

    /// A prior lesson that names a finding's file annotates and boosts it.
    #[test]
    fn prior_lesson_informs_a_finding() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "src/flaky.rs",
            "// TODO: fix the flaky path\npub fn f() {}\n",
        );

        let baseline = review(root, &ReviewOptions::default());
        let base = baseline
            .findings
            .iter()
            .find(|f| f.kind == FindingKind::Todo)
            .unwrap()
            .confidence;

        let informed = review(
            root,
            &ReviewOptions {
                prior_lessons: vec!["recurring trouble in src/flaky.rs".to_string()],
                ..ReviewOptions::default()
            },
        );
        let todo = informed
            .findings
            .iter()
            .find(|f| f.kind == FindingKind::Todo)
            .unwrap();
        assert!(
            todo.confidence > base,
            "a prior lesson should boost confidence"
        );
        assert!(todo.evidence.contains("prior lesson"), "{}", todo.evidence);
    }

    /// Friction findings join the same ranked stream as repo-scan findings.
    #[test]
    fn friction_findings_merge_into_the_ranked_report() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/a.rs", "// TODO: later\npub fn f() {}\n");
        let block = r#"[{"evidence":"used edit_file, wanted a multi-file patch tool","severity":"high","confidence":0.9}]"#;

        let report = review(
            root,
            &ReviewOptions {
                friction_block: Some(block.to_string()),
                ..ReviewOptions::default()
            },
        );
        let friction = report
            .findings
            .iter()
            .find(|f| f.kind == FindingKind::Friction)
            .expect("friction finding present");
        // High severity × 0.9 outranks the low-severity TODO, so it leads.
        assert_eq!(
            report.findings.first().map(|f| f.kind),
            Some(FindingKind::Friction)
        );
        assert_eq!(friction.severity, Severity::High);
    }

    /// Auto-captured process friction joins the same ranked stream as the repo
    /// scan and the audit-prompt friction.
    #[test]
    fn process_friction_merges_into_the_ranked_report() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "src/a.rs", "// TODO: later\npub fn f() {}\n");

        let report = review(
            root,
            &ReviewOptions {
                process: Some(ProcessFriction {
                    tool_calls: 6,
                    redundant_calls: 4, // heavy thrash -> High
                    reproduce_before_fix: true,
                    test_before_done: true,
                    exit_reason: "Done".to_string(),
                    ..ProcessFriction::default()
                }),
                ..ReviewOptions::default()
            },
        );
        let friction = report
            .findings
            .iter()
            .find(|f| f.kind == FindingKind::Friction)
            .expect("process friction finding present");
        assert_eq!(friction.severity, Severity::High);
        // High × 0.95 outranks the low-severity TODO, so it leads the report.
        assert_eq!(
            report.findings.first().map(|f| f.kind),
            Some(FindingKind::Friction)
        );
    }
}
