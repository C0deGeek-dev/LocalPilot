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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    /// The sub-question this evidence was gathered for.
    pub question: String,
    /// The gathered text.
    pub snippet: String,
    /// Where it came from.
    pub provenance: Provenance,
    /// How strongly this evidence actually matches `question`, normalized to
    /// `0.0..=1.0`. A source-specific signal (e.g. a knowledge hit's bm25
    /// relevance, a saturating curve — never a flat per-run constant), so a
    /// weak incidental match reads as less trustworthy than a strong one. The
    /// `Finding` synthesised from this evidence carries its `confidence`.
    pub relevance: f32,
    /// Provenance of near-duplicate snippets folded into this one by the
    /// loop's dedup pass — the same content found elsewhere. Kept so a folded
    /// duplicate's origin still reaches the finding's `supporting` list and
    /// the coverage account, never silently dropped.
    #[serde(default)]
    pub also_from: Vec<Provenance>,
}

impl Evidence {
    /// Construct evidence with no folded duplicates.
    #[must_use]
    pub fn new(
        question: impl Into<String>,
        snippet: impl Into<String>,
        provenance: Provenance,
        relevance: f32,
    ) -> Self {
        Self {
            question: question.into(),
            snippet: snippet.into(),
            provenance,
            relevance,
            also_from: Vec::new(),
        }
    }
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    /// The claim itself — a concise, single-line natural-language statement.
    /// Raw source text never lives here; it is carried in `evidence` so a
    /// finding reads as a claim, not a pasted code/HTML chunk.
    pub statement: String,
    /// Whether gathered evidence backs it (set by the loop's cross-check, not
    /// trusted from the synthesizer).
    pub status: ClaimStatus,
    /// The evidence backing the statement; empty implies `Unsupported`.
    pub supporting: Vec<Provenance>,
    /// The raw supporting snippet, kept separate from the claim when the source
    /// text is a code/HTML blob or too long to read as a statement. `None` when
    /// the statement already stands on its own.
    #[serde(default)]
    pub evidence: Option<String>,
    /// How much to trust this finding, normalized to `0.0..=1.0` and derived
    /// from its evidence's `relevance` — never a flat per-run constant. The
    /// binding layer clamps this under a low-trust ceiling before it reaches
    /// the review queue (findings here are machine-derived and unreviewed
    /// regardless of how strong the underlying match was).
    #[serde(default)]
    pub confidence: f32,
}

/// Collapse all runs of whitespace (including newlines) into single spaces and
/// trim the ends, so multi-line source text renders as one readable line.
#[must_use]
pub fn flatten_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// How well one sub-question ended up supported by gathered evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoverageVerdict {
    /// Enough relevant evidence from enough independent origins.
    Covered,
    /// Some evidence, but thin — few snippets or a single origin.
    Weak,
    /// No evidence at all.
    Open,
}

/// Per-sub-question coverage: the deterministic scoring the multi-round loop
/// steers by, kept on the report so a reader can judge the research, not just
/// read it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionCoverage {
    /// The sub-question.
    pub question: String,
    /// The verdict at the end of the run.
    pub verdict: CoverageVerdict,
    /// Total evidence snippets gathered for this question.
    pub evidence_count: usize,
    /// Snippets at or above the relevance floor.
    pub strong_evidence: usize,
    /// Distinct evidence origins (source label + host/locator) above the floor.
    pub distinct_origins: usize,
}

/// The full result of a research run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResearchReport {
    /// The topic that was researched.
    pub topic: String,
    /// The sub-questions the topic was decomposed into.
    pub questions: Vec<String>,
    /// The synthesised findings with provenance and support status.
    pub findings: Vec<Finding>,
    /// Sub-questions that gathered no evidence.
    pub open_questions: Vec<String>,
    /// Per-question coverage scoring from the retrieval loop.
    #[serde(default)]
    pub coverage: Vec<QuestionCoverage>,
    /// How many retrieval rounds the loop ran.
    #[serde(default)]
    pub rounds_run: usize,
    /// Loud accounting of anything the loop dropped, folded, or capped —
    /// silent truncation reads as "covered everything" when it didn't.
    #[serde(default)]
    pub retrieval_notes: Vec<String>,
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
            coverage: Vec::new(),
            rounds_run: 0,
            retrieval_notes: Vec::new(),
        }
    }
}
