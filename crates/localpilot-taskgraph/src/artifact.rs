//! The typed handoff artifact a completed task hands to its downstream nodes,
//! and the lenient confidence parser that makes it survive contact with a model.
//!
//! A task graph is only as useful as what flows along its edges. A node that
//! reports "done" and nothing else forces every downstream node to redo the
//! reading, which is the failure mode a task graph exists to prevent. So a
//! completion carries a structured artifact instead of prose, and the fields are
//! chosen so the *absent* work is as visible as the finished work:
//! [`HandoffArtifact::what_i_did_not_check`] is a required field in deep mode
//! precisely because it is the one a confident model omits.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A calibrated self-report, normalised to `0.0..=1.0`.
///
/// Stored normalised so downstream policy ("a gate needs at least 0.6") is one
/// comparison rather than a parse at every read site.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Confidence(f32);

impl Confidence {
    /// The lowest expressible confidence.
    pub const NONE: Self = Self(0.0);
    /// Full confidence.
    pub const FULL: Self = Self(1.0);

    /// Clamp `value` into `0.0..=1.0`. Out-of-range input is a model slip, not a
    /// caller bug, so it is clamped rather than rejected.
    #[must_use]
    pub fn new(value: f32) -> Self {
        Self(value.clamp(0.0, 1.0))
    }

    /// The normalised value.
    #[must_use]
    pub fn value(self) -> f32 {
        self.0
    }

    /// Whether this meets `floor` — the comparison every policy site wants.
    #[must_use]
    pub fn at_least(self, floor: Self) -> bool {
        self.0 >= floor.0
    }
}

impl fmt::Display for Confidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.2}", self.0)
    }
}

/// Why a confidence report could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ConfidenceError {
    /// Nothing was reported at all.
    #[error("no confidence was reported")]
    Missing,
    /// Something was reported, but no reading of it produced a number.
    #[error(
        "could not read {0:?} as a confidence — use a fraction (\"7/10\"), a percentage \
         (\"70%\"), a number in 0-1, or one of: none, very low, low, medium, high, very high, certain"
    )]
    Unreadable(String),
}

/// Read a model's confidence report in whatever shape it arrived in.
///
/// Models do not report confidence consistently, and a strict parser turns a
/// well-done task into a failed one over its last line. The accepted forms are
/// the ones seen in practice:
///
/// - a fraction — `7/10`, `3 / 5`
/// - a percentage — `85%`
/// - a bare number — `0.9` (already normalised), or `7` (read as `7/10`, since a
///   model that writes a bare `7` means seven out of ten, never 700%)
/// - a word — `none`, `very low`, `low`, `medium`/`moderate`, `high`,
///   `very high`, `certain`
///
/// Surrounding words are tolerated (`"confidence: high"`, `"about 7/10"`): the
/// first readable token wins.
///
/// # Errors
/// [`ConfidenceError::Missing`] when `raw` is blank, or
/// [`ConfidenceError::Unreadable`] when no reading applies.
pub fn parse_confidence(raw: &str) -> Result<Confidence, ConfidenceError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ConfidenceError::Missing);
    }
    let lower = trimmed.to_ascii_lowercase();

    // Words first: they are unambiguous, and "very low" must be matched before
    // "low" or the qualifier is silently dropped.
    for (needle, value) in WORD_SCALE {
        if lower.contains(needle) {
            return Ok(Confidence::new(*value));
        }
    }

    // A fraction written with spaces around the slash ("7 / 10") tokenises into
    // three pieces, the first of which reads as a perfectly plausible bare
    // number — so the un-spaced form has to be tried *first* or "3 / 5" is
    // silently read as 0.3.
    if lower.contains('/') {
        if let Some(found) = read_numeric(&lower.replace(' ', "")) {
            return Ok(found);
        }
    }
    for token in lower.split(|c: char| c.is_whitespace() || c == ',' || c == ';') {
        if let Some(found) = read_numeric(token) {
            return Ok(found);
        }
    }
    Err(ConfidenceError::Unreadable(trimmed.to_string()))
}

/// Word forms, longest-qualifier first so `very low` never matches as `low`.
const WORD_SCALE: &[(&str, f32)] = &[
    ("very low", 0.1),
    ("very high", 0.95),
    ("certain", 1.0),
    ("none", 0.0),
    ("low", 0.25),
    ("moderate", 0.5),
    ("medium", 0.5),
    ("high", 0.8),
];

/// Read one token as a fraction, a percentage, or a bare number.
fn read_numeric(token: &str) -> Option<Confidence> {
    let token =
        token.trim_matches(|c: char| !c.is_ascii_digit() && c != '.' && c != '/' && c != '%');
    if token.is_empty() {
        return None;
    }
    if let Some((num, den)) = token.split_once('/') {
        let num: f32 = num.trim().parse().ok()?;
        let den: f32 = den.trim().parse().ok()?;
        if den <= 0.0 {
            return None;
        }
        return Some(Confidence::new(num / den));
    }
    if let Some(percent) = token.strip_suffix('%') {
        let value: f32 = percent.trim().parse().ok()?;
        return Some(Confidence::new(value / 100.0));
    }
    let value: f32 = token.parse().ok()?;
    // A bare number above 1 is a model writing "7" for seven out of ten. Above
    // 10 it is a percentage written without its sign.
    let normalised = if value <= 1.0 {
        value
    } else if value <= 10.0 {
        value / 10.0
    } else {
        value / 100.0
    };
    Some(Confidence::new(normalised))
}

/// Accept a confidence from JSON as either a number or any of the string forms
/// [`parse_confidence`] reads, so a snapshot written by a model-facing tool
/// round-trips without a bespoke shim at every call site.
impl<'de> Deserialize<'de> for Confidence {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Number(f32),
            Text(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Number(value) => Ok(Confidence::new(value)),
            Raw::Text(text) => parse_confidence(&text).map_err(serde::de::Error::custom),
        }
    }
}

/// What one completed node hands to everything downstream of it.
///
/// The shape is the contract: a downstream node reads this instead of redoing
/// the upstream work, and a gate reviews *this* rather than the transcript. The
/// two fields that carry the most weight are the two a model most wants to skip
/// — [`evidence`](Self::evidence), which makes a claim checkable, and
/// [`what_i_did_not_check`](Self::what_i_did_not_check), which makes the gap
/// visible instead of implied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffArtifact {
    /// What the task established, in the node's own words.
    pub findings: String,
    /// Where each finding came from — paths, commands, outputs. A finding with
    /// no evidence is an assertion.
    #[serde(default)]
    pub evidence: Vec<String>,
    /// Cases the task considered and how they behave.
    #[serde(default)]
    pub edge_cases: Vec<String>,
    /// How the work was verified. Empty means "not verified", stated plainly.
    #[serde(default)]
    pub validation: String,
    /// What the task could not settle, handed on rather than dropped.
    #[serde(default)]
    pub open_questions: Vec<String>,
    /// The node's calibrated self-report.
    pub confidence: Confidence,
    /// The coverage gap, in the node's own words. Required in deep mode: it is
    /// the field that turns a confident report into an honest one.
    #[serde(default)]
    pub what_i_did_not_check: String,
}

impl HandoffArtifact {
    /// A minimal artifact: what was found and how sure the node is.
    #[must_use]
    pub fn new(findings: impl Into<String>, confidence: Confidence) -> Self {
        Self {
            findings: findings.into(),
            evidence: Vec::new(),
            edge_cases: Vec::new(),
            validation: String::new(),
            open_questions: Vec::new(),
            confidence,
            what_i_did_not_check: String::new(),
        }
    }

    /// Set the coverage gap.
    #[must_use]
    pub fn with_gap(mut self, gap: impl Into<String>) -> Self {
        self.what_i_did_not_check = gap.into();
        self
    }

    /// Add one evidence line.
    #[must_use]
    pub fn with_evidence(mut self, evidence: impl Into<String>) -> Self {
        self.evidence.push(evidence.into());
        self
    }

    /// Say how the work was verified. Required of a review gate: a gate that
    /// cannot say how it reviewed did not review.
    #[must_use]
    pub fn with_validation(mut self, validation: impl Into<String>) -> Self {
        self.validation = validation.into();
        self
    }

    /// Render for a downstream node's input. Empty sections are omitted rather
    /// than printed as headings with nothing under them — the reader is a model
    /// paying by the token.
    #[must_use]
    pub fn render(&self, label: &str) -> String {
        let mut out = format!("### {label}\n{}\n", self.findings.trim());
        push_list(&mut out, "Evidence", &self.evidence);
        push_list(&mut out, "Edge cases", &self.edge_cases);
        if !self.validation.trim().is_empty() {
            out.push_str(&format!("Validation: {}\n", self.validation.trim()));
        }
        push_list(&mut out, "Open questions", &self.open_questions);
        if !self.what_i_did_not_check.trim().is_empty() {
            out.push_str(&format!(
                "Not checked: {}\n",
                self.what_i_did_not_check.trim()
            ));
        }
        out.push_str(&format!("Confidence: {}\n", self.confidence));
        out
    }
}

fn push_list(out: &mut String, heading: &str, items: &[String]) {
    let items: Vec<&String> = items.iter().filter(|i| !i.trim().is_empty()).collect();
    if items.is_empty() {
        return;
    }
    out.push_str(heading);
    out.push_str(":\n");
    for item in items {
        out.push_str("- ");
        out.push_str(item.trim());
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_fraction() {
        assert_eq!(parse_confidence("7/10").unwrap(), Confidence::new(0.7));
        assert_eq!(parse_confidence("3 / 5").unwrap(), Confidence::new(0.6));
    }

    #[test]
    fn reads_a_percentage() {
        assert_eq!(parse_confidence("85%").unwrap(), Confidence::new(0.85));
    }

    #[test]
    fn reads_a_normalised_number() {
        assert_eq!(parse_confidence("0.9").unwrap(), Confidence::new(0.9));
    }

    #[test]
    fn reads_a_bare_number_as_out_of_ten() {
        assert_eq!(parse_confidence("7").unwrap(), Confidence::new(0.7));
    }

    #[test]
    fn reads_a_bare_number_above_ten_as_a_percentage() {
        assert_eq!(parse_confidence("85").unwrap(), Confidence::new(0.85));
    }

    #[test]
    fn very_low_is_not_read_as_low() {
        let very_low = parse_confidence("very low").unwrap();
        let low = parse_confidence("low").unwrap();
        assert!(very_low < low, "{very_low} should be under {low}");
    }

    #[test]
    fn tolerates_surrounding_words() {
        assert_eq!(
            parse_confidence("confidence: about 7/10").unwrap(),
            Confidence::new(0.7)
        );
        assert_eq!(
            parse_confidence("I am fairly high on this").unwrap(),
            Confidence::new(0.8)
        );
    }

    #[test]
    fn blank_is_missing_not_unreadable() {
        assert_eq!(parse_confidence("   "), Err(ConfidenceError::Missing));
    }

    #[test]
    fn prose_with_no_reading_is_unreadable() {
        assert!(matches!(
            parse_confidence("who can say"),
            Err(ConfidenceError::Unreadable(_))
        ));
    }

    #[test]
    fn out_of_range_clamps() {
        assert_eq!(Confidence::new(4.0), Confidence::FULL);
        assert_eq!(Confidence::new(-1.0), Confidence::NONE);
    }

    #[test]
    fn a_zero_denominator_does_not_divide() {
        assert!(parse_confidence("7/0").is_err());
    }

    #[test]
    fn deserialises_from_a_number_or_a_string() {
        let from_number: Confidence = serde_json::from_str("0.42").unwrap();
        assert_eq!(from_number, Confidence::new(0.42));
        let from_text: Confidence = serde_json::from_str("\"7/10\"").unwrap();
        assert_eq!(from_text, Confidence::new(0.7));
    }

    #[test]
    fn render_omits_empty_sections() {
        let artifact = HandoffArtifact::new("found the bug", Confidence::new(0.8));
        let rendered = artifact.render("upstream");
        assert!(rendered.contains("found the bug"));
        assert!(!rendered.contains("Evidence"));
        assert!(!rendered.contains("Not checked"));
        assert!(rendered.contains("Confidence: 0.80"));
    }

    #[test]
    fn render_includes_the_gap_when_present() {
        let artifact = HandoffArtifact::new("found it", Confidence::new(0.8))
            .with_gap("the Windows path")
            .with_evidence("src/lib.rs:12");
        let rendered = artifact.render("upstream");
        assert!(rendered.contains("Not checked: the Windows path"));
        assert!(rendered.contains("- src/lib.rs:12"));
    }
}
