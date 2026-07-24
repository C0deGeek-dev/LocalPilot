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
    /// Machine-fetchable id within the source when one exists (an ingest chunk
    /// id), so review/diagnostic surfaces can re-fetch the full source the
    /// human-readable locator points at. `None` for sources without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetch_id: Option<String>,
}

impl Provenance {
    /// Construct a provenance tag.
    #[must_use]
    pub fn new(source: impl Into<String>, locator: Option<String>) -> Self {
        Self {
            source: source.into(),
            locator,
            fetch_id: None,
        }
    }

    /// Attach a machine-fetchable id.
    #[must_use]
    pub fn with_fetch_id(mut self, fetch_id: impl Into<String>) -> Self {
        self.fetch_id = Some(fetch_id.into());
        self
    }
}

/// How one evidence item's final `relevance` was decided — the admission
/// trail. Kept for diagnostics (rendered content-free in the report's
/// retrieval accounting), so "high relevance" is auditable: within-source
/// rank and question-level admission are different judgments and must not be
/// conflated (LocalHub#32).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdmissionTrail {
    /// The source engine's raw signal (e.g. bm25-derived unit relevance).
    pub raw: f32,
    /// Rank relative to the query's best hit within this source — ordering
    /// only, never the admission value.
    pub rank: f32,
    /// How the final `relevance` was decided, e.g. `model admission`,
    /// `term overlap`, `reviewed memory`.
    pub reason: String,
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
    /// The full bounded source text behind `snippet`, when the source holds
    /// more than the match window (a local chunk body). `None` when the
    /// snippet already is the full gathered content (a fetched web page) or
    /// the source cannot supply more. Review-only context: it rides into the
    /// finding's `evidence`, never into the claim itself (LocalHub#34).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_source: Option<String>,
    /// How `relevance` was decided, for diagnostics. `None` for sources that
    /// predate the trail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<AdmissionTrail>,
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
            full_source: None,
            admission: None,
        }
    }

    /// Attach the full bounded source text behind the snippet.
    #[must_use]
    pub fn with_full_source(mut self, full_source: impl Into<String>) -> Self {
        self.full_source = Some(full_source.into());
        self
    }

    /// Attach the admission trail explaining `relevance`.
    #[must_use]
    pub fn with_admission(mut self, admission: AdmissionTrail) -> Self {
        self.admission = Some(admission);
        self
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
    /// Enough relevant evidence from enough independent origins across at
    /// least two source families — independently corroborated.
    Covered,
    /// Enough relevant evidence and locator diversity, but every observation
    /// comes from one source family (e.g. two files of one repository, or two
    /// pages of one host) — supported, not independently corroborated
    /// (LocalHub#33). Distinct from [`CoverageVerdict::Covered`] so file
    /// diversity can never read as cross-source validation.
    CoveredSingleSource,
    /// Some evidence, but thin — few snippets or a single origin.
    Weak,
    /// No evidence at all.
    Open,
}

/// Per-source retrieval account for one sub-question, aggregated across
/// rounds and reformulated queries. Counts and reasons only — never source
/// content or unredacted queries. This is what lets a reader of the report
/// tell "web proposed nothing" from "web fetched and was rejected"
/// (LocalHub#33).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceAccount {
    /// The source label (`knowledge`, `memory`, `web`).
    pub source: String,
    /// Candidates considered: hits returned by the index, URLs proposed.
    pub proposed: usize,
    /// Evidence items handed to the engine's pool.
    pub admitted: usize,
    /// Rejected by the source's own relevance admission (e.g. the model
    /// classifier) before reaching the pool.
    pub rejected_relevance: usize,
    /// Skipped by policy: non-allowlisted host, host cooldown, no consent.
    pub policy_skipped: usize,
    /// Redirect responses, never followed.
    pub redirected: usize,
    /// Fetch/read failures and unsuccessful responses (including a source
    /// call that errored outright).
    pub failed: usize,
    /// Pool evidence that fell below the engine's admission floor.
    pub below_floor: usize,
    /// Fetched pages whose static HTML showed a render signal (a client-rendered
    /// shell, hydration-only markup, or an iframe-only body) that the static
    /// path cannot fully extract — an explicit, inspectable outcome so a page
    /// that needed rendering is never silently counted as complete
    /// (LocalHub#37). Zero on the ordinary server-rendered path.
    #[serde(default)]
    pub render_required: usize,
    /// Content-free per-admitted-item diagnostics: locator plus the admission
    /// trail (raw signal, within-source rank, final relevance, reason).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub admitted_notes: Vec<String>,
    /// Content-free render diagnostics: one short line per fetched page that
    /// showed a render signal — the signal reason and what became of it
    /// (frame recovered, renderer unavailable). Never page content.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub render_notes: Vec<String>,
}

impl SourceAccount {
    /// An empty account for `source`.
    #[must_use]
    pub fn new(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            ..Self::default()
        }
    }

    /// Fold another account (same source, later query/round) into this one.
    pub fn merge(&mut self, other: &SourceAccount) {
        self.proposed += other.proposed;
        self.admitted += other.admitted;
        self.rejected_relevance += other.rejected_relevance;
        self.policy_skipped += other.policy_skipped;
        self.redirected += other.redirected;
        self.failed += other.failed;
        self.below_floor += other.below_floor;
        self.render_required += other.render_required;
        self.admitted_notes
            .extend(other.admitted_notes.iter().cloned());
        self.render_notes.extend(other.render_notes.iter().cloned());
    }
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
    /// Distinct source families above the floor: each web host is its own
    /// family; every non-web source is one family per label. Locator
    /// diversity within one family (two files of one repository) never
    /// raises this (LocalHub#33).
    #[serde(default)]
    pub distinct_families: usize,
    /// Per-source retrieval accounting for this question, sorted by source.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<SourceAccount>,
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
    /// Whether the run had web research enabled — set by the binding layer,
    /// `None` when unknown (older reports). Lets the renderer mark a
    /// question that web contributed nothing to as a source gap instead of
    /// presenting local-only coverage as cross-validated (LocalHub#33).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_enabled: Option<bool>,
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
            web_enabled: None,
        }
    }
}
