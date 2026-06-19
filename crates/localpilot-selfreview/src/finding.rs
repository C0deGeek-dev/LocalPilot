//! The finding contract and the ranked report.
//!
//! A [`Finding`] is one observed, advisory repo-health signal. Findings are
//! ranked by **severity × confidence** into a stable [`Report`] that renders both
//! a machine-readable JSON form and a human summary. Nothing here writes to the
//! repository — a finding is data the reader acts on, never an action.

use serde::{Deserialize, Serialize};

/// Schema tag for the machine-readable report, so a consumer can pin the shape.
pub const REPORT_SCHEMA: &str = "localpilot-selfreview-v1";

/// What kind of repo-health signal a finding represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// A `TODO`/`FIXME`/`XXX`/`HACK` marker left in a tracked file.
    Todo,
    /// A decision index (e.g. a registry) lags the actual decision log.
    StaleAdr,
    /// A plan/tracking document carries an unresolved or incomplete row.
    BrokenPlan,
    /// A source file with public API and no co-located tests (heuristic).
    MissingTest,
    /// A document references a local file that does not exist (broken link).
    DocDrift,
    /// A friction finding emitted by a model auditing the harness during work.
    Friction,
}

impl FindingKind {
    /// Stable ordinal for deterministic tie-breaking in the ranked report.
    fn ordinal(self) -> u8 {
        match self {
            FindingKind::StaleAdr => 0,
            FindingKind::BrokenPlan => 1,
            FindingKind::DocDrift => 2,
            FindingKind::Friction => 3,
            FindingKind::MissingTest => 4,
            FindingKind::Todo => 5,
        }
    }
}

/// How serious a finding is. Ordered low → high; the weight feeds the rank score.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
}

impl Severity {
    /// The rank weight, multiplied by confidence to produce the score.
    #[must_use]
    pub fn weight(self) -> f32 {
        match self {
            Severity::Info => 1.0,
            Severity::Low => 2.0,
            Severity::Medium => 3.0,
            Severity::High => 4.0,
        }
    }
}

/// A 1-based inclusive line span within a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start_line: u64,
    pub end_line: u64,
}

impl Span {
    /// A single-line span.
    #[must_use]
    pub fn line(line: u64) -> Self {
        Self {
            start_line: line,
            end_line: line,
        }
    }
}

/// One advisory repo-health finding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    /// The kind of signal.
    pub kind: FindingKind,
    /// The file the finding is about (project-relative), when it has one.
    pub path: Option<String>,
    /// The line span within `path`, when known.
    pub span: Option<Span>,
    /// Seriousness.
    pub severity: Severity,
    /// Confidence in `[0.0, 1.0]` — how sure the detector is this is real.
    pub confidence: f32,
    /// A short, human-readable description grounding the finding in evidence.
    pub evidence: String,
    /// Who is best placed to act on it (a role hint), when known.
    pub suggested_owner: Option<String>,
}

impl Finding {
    /// Build a finding, clamping confidence into `[0.0, 1.0]`.
    #[must_use]
    pub fn new(kind: FindingKind, severity: Severity, confidence: f32, evidence: String) -> Self {
        Self {
            kind,
            path: None,
            span: None,
            severity,
            confidence: confidence.clamp(0.0, 1.0),
            evidence,
            suggested_owner: None,
        }
    }

    /// Attach a file path.
    #[must_use]
    pub fn at_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Attach a line span.
    #[must_use]
    pub fn at_span(mut self, span: Span) -> Self {
        self.span = Some(span);
        self
    }

    /// Attach a suggested owner role.
    #[must_use]
    pub fn owned_by(mut self, owner: impl Into<String>) -> Self {
        self.suggested_owner = Some(owner.into());
        self
    }

    /// The rank score: severity weight × confidence. Higher ranks first.
    #[must_use]
    pub fn score(&self) -> f32 {
        self.severity.weight() * self.confidence
    }
}

/// A ranked, advisory self-review report. Read-only by construction: it holds
/// findings and counts, and writes nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    /// Schema tag (`REPORT_SCHEMA`).
    pub schema: String,
    /// Findings in ranked order (highest severity × confidence first).
    pub findings: Vec<Finding>,
    /// How many files the scan read.
    pub scanned_files: usize,
}

impl Report {
    /// Build a ranked report from unranked findings. Ranking is deterministic:
    /// by score descending, then by kind ordinal, path, and span start, so the
    /// same repo always yields the same report (a property tests rely on).
    #[must_use]
    pub fn ranked(mut findings: Vec<Finding>, scanned_files: usize) -> Self {
        findings.sort_by(|a, b| {
            b.score()
                .partial_cmp(&a.score())
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.kind.ordinal().cmp(&b.kind.ordinal()))
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| {
                    a.span
                        .map(|s| s.start_line)
                        .cmp(&b.span.map(|s| s.start_line))
                })
                .then_with(|| a.evidence.cmp(&b.evidence))
        });
        Self {
            schema: REPORT_SCHEMA.to_string(),
            findings,
            scanned_files,
        }
    }

    /// Serialize the report as stable, machine-readable JSON.
    ///
    /// # Errors
    /// Returns a [`serde_json::Error`] only if serialization fails (it does not
    /// for this owned, finite structure).
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// A compact human summary: a count line then one line per finding.
    #[must_use]
    pub fn human_summary(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "self-review: {} finding(s) across {} file(s)",
            self.findings.len(),
            self.scanned_files
        );
        for finding in &self.findings {
            let location = match (&finding.path, &finding.span) {
                (Some(path), Some(span)) => format!("{path}:{}", span.start_line),
                (Some(path), None) => path.clone(),
                _ => "-".to_string(),
            };
            let _ = writeln!(
                out,
                "- [{:?}/{:?} {:.2}] {location}: {}",
                finding.severity, finding.kind, finding.confidence, finding.evidence
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(severity: Severity, confidence: f32) -> Finding {
        Finding::new(FindingKind::Todo, severity, confidence, "e".to_string())
    }

    #[test]
    fn score_is_severity_times_confidence_and_confidence_is_clamped() {
        assert!((finding(Severity::High, 0.5).score() - 2.0).abs() < f32::EPSILON);
        // Out-of-range confidence is clamped at construction.
        assert!(
            (Finding::new(FindingKind::Todo, Severity::Low, 5.0, String::new()).confidence - 1.0)
                .abs()
                < f32::EPSILON
        );
    }

    #[test]
    fn ranked_orders_by_score_descending() {
        let report = Report::ranked(
            vec![
                finding(Severity::Low, 0.9),    // score 1.8
                finding(Severity::High, 0.9),   // score 3.6
                finding(Severity::Medium, 0.5), // score 1.5
            ],
            3,
        );
        let scores: Vec<f32> = report.findings.iter().map(Finding::score).collect();
        assert!(
            scores[0] >= scores[1] && scores[1] >= scores[2],
            "{scores:?}"
        );
        assert!((scores[0] - 3.6).abs() < 0.001);
    }

    #[test]
    fn report_round_trips_through_json() {
        let report = Report::ranked(vec![finding(Severity::Medium, 0.7)], 1);
        let json = report.to_json().unwrap();
        let parsed: Report = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, report);
        assert!(json.contains(REPORT_SCHEMA));
    }
}
