//! Value types describing a research run's inputs and output.
//!
//! These types are host-neutral: they carry no filesystem, network, or engine
//! handles, so the loop that produces them can be unit-tested with fake
//! sources and a deterministic synthesizer.

use serde::{Deserialize, Serialize};

/// Where a piece of evidence — or a finding's support — came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// Stable label of the producing source, e.g. `memory`, `knowledge`, `web`.
    pub source: String,
    /// Locator within that source when one exists: a memory id, `path:start-end`,
    /// or a URL. `None` when the source cannot point at a sub-location.
    pub locator: Option<String>,
}

impl Provenance {
    /// Construct a provenance tag.
    #[must_use]
    pub fn new(source: impl Into<String>, locator: Option<String>) -> Self {
        Self {
            source: source.into(),
            locator,
        }
    }
}

/// A raw snippet gathered from a source in answer to a sub-question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// The sub-question this evidence was gathered for.
    pub question: String,
    /// The gathered text.
    pub snippet: String,
    /// Where it came from.
    pub provenance: Provenance,
}

/// How well a synthesised finding is backed by gathered evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimStatus {
    /// At least one piece of evidence backs the statement.
    Supported,
    /// No gathered evidence backs the statement — surfaced, never hidden.
    Unsupported,
}

/// A synthesised statement about the topic with its supporting provenance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// The claim itself.
    pub statement: String,
    /// Whether gathered evidence backs it (set by the loop's cross-check, not
    /// trusted from the synthesizer).
    pub status: ClaimStatus,
    /// The evidence backing the statement; empty implies `Unsupported`.
    pub supporting: Vec<Provenance>,
}

/// The full result of a research run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResearchReport {
    /// The topic that was researched.
    pub topic: String,
    /// The sub-questions the topic was decomposed into.
    pub questions: Vec<String>,
    /// The synthesised findings with provenance and support status.
    pub findings: Vec<Finding>,
    /// Sub-questions that gathered no evidence.
    pub open_questions: Vec<String>,
}

impl ResearchReport {
    /// An empty report for `topic`.
    #[must_use]
    pub fn new(topic: impl Into<String>) -> Self {
        Self {
            topic: topic.into(),
            questions: Vec::new(),
            findings: Vec::new(),
            open_questions: Vec::new(),
        }
    }
}
