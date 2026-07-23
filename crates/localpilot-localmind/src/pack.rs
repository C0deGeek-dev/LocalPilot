//! Cross-source context-pack budget allocation.
//!
//! A context pack draws from several derived sources — accepted memory anchors,
//! recent session facts, ingest hits, and code-graph neighbors — that must
//! *compete under one token budget* rather than each getting a fixed slice that
//! crowds the others out. Allocation is two-phase and fully deterministic:
//!
//! 1. **Reserves.** Each source is guaranteed up to a small fraction of the
//!    budget, filled highest-trust source first and highest score within a
//!    source. This keeps a flood of ingest hits from starving a single
//!    high-value accepted-memory anchor.
//! 2. **Shared pool.** Whatever budget the reserves leave is filled by global
//!    score across every remaining candidate, so a strong hit from any source
//!    can still win the leftover space.
//!
//! Every candidate ends up either selected or skipped *with a reason*, so a pack
//! is always inspectable: why each entry is in, and why a high-ranking near-miss
//! is out.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Where a context-pack candidate originated. Declaration order is the
/// reserve-fill priority: earlier sources are filled first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PackSource {
    /// A context entry the user explicitly pinned; highest precedence.
    ManualPin,
    /// An accepted, review-gated LocalMind memory.
    AcceptedMemory,
    /// A fact carried from the recent session (compaction digest, etc.).
    RecentSession,
    /// A derived ingest chunk matching the task query.
    Ingest,
    /// A code-graph neighbor of a task-relevant symbol.
    CodeGraph,
}

impl PackSource {
    /// Fill priority within the reserve phase (lower wins).
    fn priority(self) -> u8 {
        match self {
            PackSource::ManualPin => 0,
            PackSource::AcceptedMemory => 1,
            PackSource::RecentSession => 2,
            PackSource::Ingest => 3,
            PackSource::CodeGraph => 4,
        }
    }

    /// Source-quality weight contributed to a candidate's rank. Trusted,
    /// review-gated, or user-pinned sources outrank lexical ingest hits.
    fn quality_weight(self) -> i64 {
        match self {
            PackSource::ManualPin => 40,
            PackSource::AcceptedMemory => 30,
            PackSource::RecentSession => 20,
            PackSource::Ingest => 10,
            PackSource::CodeGraph => 5,
        }
    }

    /// Fraction of the total budget reserved for this source. Reserves
    /// deliberately sum to less than one so a shared pool remains for global
    /// competition.
    fn reserve_fraction(self) -> f64 {
        match self {
            PackSource::ManualPin => 0.15,
            PackSource::AcceptedMemory => 0.20,
            PackSource::RecentSession => 0.15,
            PackSource::Ingest => 0.25,
            PackSource::CodeGraph => 0.10,
        }
    }

    /// Minimum unit relevance a candidate needs before it may *fill this
    /// source's reserve*. A guaranteed budget share is for content that shows
    /// some relationship to the task — an ineligible candidate still competes
    /// in the shared pool (where stronger candidates outrank it), so recall is
    /// not lost, but its source's reserve stays unused for the pool instead of
    /// being stuffed with noise. Manual pins are user-chosen and code-graph
    /// rows are already derived from task symbols, so neither is floored.
    fn reserve_floor(self) -> f32 {
        match self {
            PackSource::ManualPin => 0.0,
            PackSource::AcceptedMemory => 0.05,
            PackSource::RecentSession => 0.25,
            PackSource::Ingest => 0.05,
            PackSource::CodeGraph => 0.0,
        }
    }

    /// Every source, for reserve accounting and reporting.
    pub(crate) fn all() -> [PackSource; 5] {
        [
            PackSource::ManualPin,
            PackSource::AcceptedMemory,
            PackSource::RecentSession,
            PackSource::Ingest,
            PackSource::CodeGraph,
        ]
    }
}

/// One candidate competing for space in a context pack.
#[derive(Debug, Clone)]
pub struct PackCandidate {
    pub source: PackSource,
    pub id: String,
    pub path: Option<String>,
    /// Raw relevance score from the originating search/index. Kept for
    /// diagnostics only — raw scores from different sources are on different
    /// scales and are never compared across sources (see `relevance`).
    pub score: u64,
    /// Cross-source relevance in `0.0..=1.0` — each source maps its own raw
    /// scale onto this one bounded range (bm25 through a saturating curve,
    /// cosine as-is, lexical task overlap for session facts, fixed moderate
    /// values for graph rows, `1.0` for a user pin), preserving the source's
    /// internal order. This, not `score`, is what competes across sources.
    pub relevance: f32,
    pub token_estimate: u64,
    pub snippet: String,
    pub stale: bool,
    /// Recency rank (higher is more recent). Recent-session and current-turn
    /// candidates set this; static derived sources leave it zero.
    pub recency: u64,
    /// The task explicitly names this candidate's file path.
    pub file_match: bool,
    /// Source confidence in `0.0..=1.0` (accepted memory and extraction set
    /// this; lexical sources leave it at the neutral default).
    pub confidence: f32,
    /// Code-graph distance from a task-relevant symbol (0 = the symbol itself,
    /// 1 = a direct neighbor). Only meaningful for [`PackSource::CodeGraph`].
    pub graph_proximity: u32,
}

/// The components of a candidate's composite rank, kept so a pack is auditable:
/// a reader can see exactly why one entry outranked another.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankSignals {
    /// Normalized cross-source relevance points (`0..=RELEVANCE_POINTS`),
    /// derived from the candidate's unit relevance — the only relevance value
    /// compared across sources.
    pub relevance: i64,
    /// The source's raw score, carried for diagnostics only. Raw scales
    /// differ per source (bm25 fixed-point, FTS integers, fixed graph
    /// values); never compare this across sources.
    #[serde(default)]
    pub raw_relevance: i64,
    pub source_quality: i64,
    pub recency: i64,
    pub file_match: i64,
    pub confidence: i64,
    pub graph_proximity: i64,
    pub stale_penalty: i64,
    pub redundancy_penalty: i64,
    pub final_score: i64,
}

/// A candidate after allocation, carrying the reason it was kept or skipped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackEntry {
    pub source: PackSource,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub score: u64,
    pub token_estimate: u64,
    pub snippet: String,
    pub stale: bool,
    /// Human-readable inclusion or skip reason.
    pub reason: String,
    /// The rank-signal breakdown that decided this entry's competition.
    #[serde(default)]
    pub signals: RankSignals,
}

impl PackCandidate {
    fn into_entry(self, reason: String, signals: RankSignals) -> PackEntry {
        PackEntry {
            source: self.source,
            id: self.id,
            path: self.path,
            score: self.score,
            token_estimate: self.token_estimate,
            snippet: self.snippet,
            stale: self.stale,
            reason,
            signals,
        }
    }

    /// Dedup key: same path and same leading snippet text is the same content,
    /// even across sources.
    fn dedup_key(&self) -> String {
        let snippet: String = self
            .snippet
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(80)
            .collect::<String>()
            .to_ascii_lowercase();
        format!("{}|{snippet}", self.path.as_deref().unwrap_or(""))
    }
}

/// The outcome of competing candidates under one budget.
#[derive(Debug, Clone, Default)]
pub struct Allocation {
    pub selected: Vec<PackEntry>,
    pub skipped: Vec<PackEntry>,
    pub token_estimate: u64,
    pub per_source_tokens: BTreeMap<PackSource, u64>,
}

/// Reserve token amounts per source for `budget`.
pub(crate) fn reserves(budget: u64) -> BTreeMap<PackSource, u64> {
    PackSource::all()
        .into_iter()
        .map(|source| {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let amount = (budget as f64 * source.reserve_fraction()) as u64;
            (source, amount)
        })
        .collect()
}

/// Penalty applied to each repeat of a file path already seen higher in the
/// ranking, so repeated files/memories collapse toward one useful entry.
const REDUNDANCY_PENALTY: i64 = 15;
/// Penalty for a stale (superseded) candidate, so newer evidence wins.
const STALE_PENALTY: i64 = 25;

/// Compete `candidates` for `budget` tokens. Each candidate is scored from
/// explicit signals (relevance, source quality, recency, stale and redundancy
/// penalties), then selected reserve-first and shared-by-rank. Deterministic:
/// ties break by source priority then id.
pub(crate) fn allocate(candidates: Vec<PackCandidate>, budget: u64) -> Allocation {
    let signals = rank_all(&candidates);

    // Indices sorted for the reserve phase: source precedence, then rank, then id.
    let mut by_reserve: Vec<usize> = (0..candidates.len()).collect();
    by_reserve.sort_by(|&a, &b| {
        candidates[a]
            .source
            .priority()
            .cmp(&candidates[b].source.priority())
            .then(signals[b].final_score.cmp(&signals[a].final_score))
            .then_with(|| candidates[a].id.cmp(&candidates[b].id))
    });

    let reserves = reserves(budget);
    let mut allocation = Allocation::default();
    let mut used_total = 0_u64;
    let mut used_by_source: BTreeMap<PackSource, u64> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut selected = BTreeSet::new();
    let mut duplicate = BTreeSet::new();

    // Phase 1: reserves. Fill each source up to its guaranteed share — but
    // only with candidates clearing the source's relevance floor: a reserve
    // guarantees room for *relevant* trusted content, it is not a quota to be
    // stuffed with noise. Below-floor candidates fall through to the shared
    // pool, leaving the unused reserve to global competition.
    for &idx in &by_reserve {
        let candidate = &candidates[idx];
        if !seen.insert(candidate.dedup_key()) {
            duplicate.insert(idx);
            allocation.skipped.push(
                candidate
                    .clone()
                    .into_entry("duplicate content".to_string(), signals[idx]),
            );
            continue;
        }
        if candidate.relevance < candidate.source.reserve_floor() {
            continue;
        }
        let reserve = reserves.get(&candidate.source).copied().unwrap_or(0);
        let src_used = used_by_source.get(&candidate.source).copied().unwrap_or(0);
        let cost = candidate.token_estimate;
        if src_used.saturating_add(cost) <= reserve && used_total.saturating_add(cost) <= budget {
            used_total = used_total.saturating_add(cost);
            *used_by_source.entry(candidate.source).or_default() += cost;
            selected.insert(idx);
            allocation.selected.push(candidate.clone().into_entry(
                format!("included from {} within reserve", label(candidate.source)),
                signals[idx],
            ));
        }
    }

    // Phase 2: shared pool. Compete leftovers globally by rank.
    let mut leftovers: Vec<usize> = (0..candidates.len())
        .filter(|idx| !selected.contains(idx) && !duplicate.contains(idx))
        .collect();
    leftovers.sort_by(|&a, &b| {
        signals[b]
            .final_score
            .cmp(&signals[a].final_score)
            .then(
                candidates[a]
                    .source
                    .priority()
                    .cmp(&candidates[b].source.priority()),
            )
            .then_with(|| candidates[a].id.cmp(&candidates[b].id))
    });
    for idx in leftovers {
        let candidate = &candidates[idx];
        let cost = candidate.token_estimate;
        if used_total.saturating_add(cost) <= budget {
            used_total = used_total.saturating_add(cost);
            *used_by_source.entry(candidate.source).or_default() += cost;
            allocation.selected.push(candidate.clone().into_entry(
                format!(
                    "included from {} within shared budget",
                    label(candidate.source)
                ),
                signals[idx],
            ));
        } else {
            allocation.skipped.push(candidate.clone().into_entry(
                format!("skipped: budget exhausted ({cost} tokens did not fit)"),
                signals[idx],
            ));
        }
    }

    allocation.token_estimate = used_total;
    allocation.per_source_tokens = used_by_source;
    allocation
}

/// Score every candidate. Redundancy is counted over a canonical rank order so a
/// repeated file path is demoted on its second and later appearances.
fn rank_all(candidates: &[PackCandidate]) -> Vec<RankSignals> {
    let base: Vec<RankSignals> = candidates.iter().map(base_signals).collect();
    let mut order: Vec<usize> = (0..candidates.len()).collect();
    order.sort_by(|&a, &b| {
        base[b]
            .final_score
            .cmp(&base[a].final_score)
            .then(
                candidates[a]
                    .source
                    .priority()
                    .cmp(&candidates[b].source.priority()),
            )
            .then_with(|| candidates[a].id.cmp(&candidates[b].id))
    });
    let mut path_seen: BTreeMap<String, i64> = BTreeMap::new();
    let mut signals = base;
    for &idx in &order {
        let path = candidates[idx].path.clone().unwrap_or_default();
        let repeats = *path_seen.get(&path).unwrap_or(&0);
        let penalty = if path.is_empty() {
            0
        } else {
            repeats * REDUNDANCY_PENALTY
        };
        path_seen.insert(path, repeats + 1);
        signals[idx].redundancy_penalty = -penalty;
        signals[idx].final_score -= penalty;
    }
    signals
}

/// Bonus for a task-named file path.
const FILE_MATCH_BONUS: i64 = 20;
/// Distance at which a code-graph neighbor stops contributing proximity.
const GRAPH_PROXIMITY_REACH: i64 = 3;
/// Points a full-relevance candidate contributes — the one documented range
/// every source's relevance is mapped onto before the cross-source
/// comparison. Sized so relevance leads the rank while the bounded bonuses
/// (source quality 5..40, recency ≤50, file match 20, confidence ≤15,
/// proximity ≤9) stay measurable: a strong bonus stack can lift a candidate
/// past a modestly-more-relevant one, but never past a decisively more
/// relevant one.
const RELEVANCE_POINTS: f32 = 200.0;

/// The order-independent part of a candidate's rank. Relevance enters as the
/// candidate's normalized unit value scaled to [`RELEVANCE_POINTS`]; the raw
/// source score rides along for diagnostics only.
fn base_signals(candidate: &PackCandidate) -> RankSignals {
    #[allow(clippy::cast_possible_truncation)]
    let relevance = (candidate.relevance.clamp(0.0, 1.0) * RELEVANCE_POINTS) as i64;
    let raw_relevance = i64::try_from(candidate.score).unwrap_or(i64::MAX);
    let source_quality = candidate.source.quality_weight();
    let recency = i64::try_from(candidate.recency).unwrap_or(i64::MAX).min(50);
    let file_match = if candidate.file_match {
        FILE_MATCH_BONUS
    } else {
        0
    };
    #[allow(clippy::cast_possible_truncation)]
    let confidence = (candidate.confidence.clamp(0.0, 1.0) * 15.0) as i64;
    let graph_proximity = if candidate.source == PackSource::CodeGraph {
        (GRAPH_PROXIMITY_REACH - i64::from(candidate.graph_proximity)).max(0) * 3
    } else {
        0
    };
    let stale_penalty = if candidate.stale { -STALE_PENALTY } else { 0 };
    RankSignals {
        relevance,
        raw_relevance,
        source_quality,
        recency,
        file_match,
        confidence,
        graph_proximity,
        stale_penalty,
        redundancy_penalty: 0,
        final_score: relevance
            + source_quality
            + recency
            + file_match
            + confidence
            + graph_proximity
            + stale_penalty,
    }
}

fn label(source: PackSource) -> &'static str {
    match source {
        PackSource::ManualPin => "manual pin",
        PackSource::AcceptedMemory => "accepted memory",
        PackSource::RecentSession => "recent session",
        PackSource::Ingest => "ingest",
        PackSource::CodeGraph => "code graph",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(source: PackSource, id: &str, score: u64, tokens: u64) -> PackCandidate {
        PackCandidate {
            source,
            id: id.to_string(),
            path: Some(format!("{id}.rs")),
            score,
            // Fixture mapping: raw score 0..100 reads as unit relevance
            // 0.0..=1.0, so tests state relevance directly through `score`.
            #[allow(clippy::cast_precision_loss)]
            relevance: (score as f32 / 100.0).clamp(0.0, 1.0),
            token_estimate: tokens,
            snippet: format!("snippet {id}"),
            stale: false,
            recency: 0,
            file_match: false,
            confidence: 1.0,
            graph_proximity: 0,
        }
    }

    #[test]
    fn reserves_sum_to_less_than_the_budget() {
        let reserves = reserves(1_000);
        let total: u64 = reserves.values().sum();
        assert!(total < 1_000, "a shared pool must remain, got {total}");
    }

    #[test]
    fn every_candidate_is_selected_or_skipped() {
        let candidates = vec![
            candidate(PackSource::Ingest, "a", 10, 30),
            candidate(PackSource::AcceptedMemory, "b", 5, 30),
            candidate(PackSource::CodeGraph, "c", 8, 30),
        ];
        let n = candidates.len();
        let out = allocate(candidates, 1_000);
        assert_eq!(out.selected.len() + out.skipped.len(), n);
        assert!(out.selected.iter().all(|e| !e.reason.is_empty()));
    }

    #[test]
    fn a_reserve_protects_a_high_value_anchor_from_an_ingest_flood() {
        // Many cheap ingest hits plus one *relevant* accepted-memory anchor;
        // the anchor must survive on its reserve even though ingest has far
        // more hits.
        let mut candidates = vec![candidate(PackSource::AcceptedMemory, "anchor", 30, 20)];
        for i in 0..50 {
            candidates.push(candidate(PackSource::Ingest, &format!("i{i}"), 100, 20));
        }
        let out = allocate(candidates, 100);
        assert!(
            out.selected
                .iter()
                .any(|e| e.source == PackSource::AcceptedMemory),
            "the anchor must be protected by its reserve"
        );
    }

    #[test]
    fn a_below_floor_candidate_cannot_claim_its_reserve() {
        // The same flood, but the anchor shows essentially no relationship to
        // the task: the reserve is for relevant trusted content, so the weak
        // anchor gets no guaranteed slot and the budget goes to relevant hits.
        let mut candidates = vec![candidate(PackSource::AcceptedMemory, "anchor", 1, 20)];
        for i in 0..50 {
            candidates.push(candidate(PackSource::Ingest, &format!("i{i}"), 100, 20));
        }
        let out = allocate(candidates, 100);
        assert!(
            out.selected.iter().all(|e| e.source == PackSource::Ingest),
            "a below-floor anchor must not consume reserve or beat relevant hits: {:?}",
            out.selected
                .iter()
                .map(|e| (&e.id, e.source))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_irrelevant_session_flood_leaves_its_reserve_to_the_pool() {
        // Recent-session candidates below their floor never fill the session
        // reserve; the freed budget goes to relevant candidates in the pool.
        let mut candidates = Vec::new();
        for i in 0..3 {
            // Unit relevance 0.10 — below the session floor of 0.25.
            candidates.push(candidate(
                PackSource::RecentSession,
                &format!("s{i}"),
                10,
                20,
            ));
        }
        for i in 0..5 {
            candidates.push(candidate(PackSource::Ingest, &format!("i{i}"), 90, 20));
        }
        let out = allocate(candidates, 100);
        assert_eq!(out.selected.len(), 5);
        assert!(out.selected.iter().all(|e| e.source == PackSource::Ingest));
    }

    #[test]
    fn the_shared_pool_goes_to_the_highest_score() {
        // Two sources, tight budget: after reserves, the leftover goes to the
        // highest-scoring candidate regardless of source.
        let candidates = vec![
            candidate(PackSource::Ingest, "low", 1, 40),
            candidate(PackSource::CodeGraph, "high", 99, 40),
        ];
        let out = allocate(candidates, 60);
        // Budget 60: reserves are ingest 18, code graph 6; neither fits a 40 in
        // reserve, so the shared pool (60) takes exactly one — the higher score.
        assert_eq!(out.selected.len(), 1);
        assert_eq!(out.selected[0].id, "high");
        assert!(out.selected[0].reason.contains("shared budget"));
        assert_eq!(out.skipped.len(), 1);
        assert!(out.skipped[0].reason.contains("budget exhausted"));
    }

    #[test]
    fn duplicates_across_sources_are_skipped_once() {
        let mut a = candidate(PackSource::Ingest, "a", 10, 10);
        let mut b = candidate(PackSource::AcceptedMemory, "b", 10, 10);
        a.path = Some("same.rs".to_string());
        b.path = Some("same.rs".to_string());
        a.snippet = "identical body text".to_string();
        b.snippet = "identical body text".to_string();
        let out = allocate(vec![a, b], 1_000);
        assert_eq!(out.selected.len(), 1);
        assert_eq!(out.skipped.len(), 1);
        assert_eq!(out.skipped[0].reason, "duplicate content");
    }

    #[test]
    fn allocation_never_exceeds_the_budget() {
        let candidates: Vec<_> = (0..20)
            .map(|i| candidate(PackSource::Ingest, &format!("i{i}"), i, 25))
            .collect();
        let out = allocate(candidates, 100);
        assert!(out.token_estimate <= 100);
        let summed: u64 = out.selected.iter().map(|e| e.token_estimate).sum();
        assert_eq!(summed, out.token_estimate);
    }

    #[test]
    fn accepted_memory_quality_can_outrank_a_higher_raw_ingest_score() {
        // Tight shared budget for one slot: an accepted memory that is
        // modestly less relevant still beats an ingest hit because of its
        // source-quality weight — the bonus is bounded, so it closes small
        // relevance gaps, never large ones.
        let memory = candidate(PackSource::AcceptedMemory, "m", 15, 40); // 30 rel + 30 quality
        let ingest = candidate(PackSource::Ingest, "i", 20, 40); // 40 rel + 10 quality
        let out = allocate(vec![ingest, memory], 40);
        assert_eq!(out.selected.len(), 1);
        assert_eq!(out.selected[0].id, "m");
        assert!(out.selected[0].signals.source_quality >= 30);
    }

    #[test]
    fn relevance_dominates_a_full_bonus_stack_across_sources() {
        // A decisively more relevant candidate cannot be overturned by the
        // combined bounded bonuses of a weaker one — normalization keeps
        // relevance the lead signal.
        let strong = candidate(PackSource::Ingest, "strong", 90, 40); // 180 rel + 10 + 15
        let mut weak = candidate(PackSource::AcceptedMemory, "weak", 20, 40); // 40 rel + 30 + 15
        weak.file_match = true; // +20
        weak.recency = 50; // +50
        let out = allocate(vec![weak, strong], 40);
        assert_eq!(out.selected.len(), 1);
        assert_eq!(out.selected[0].id, "strong");
    }

    #[test]
    fn a_stale_candidate_loses_to_fresher_evidence() {
        let mut stale = candidate(PackSource::Ingest, "old", 30, 40);
        stale.stale = true; // 30 + 10 - 25 = 15
        let fresh = candidate(PackSource::Ingest, "new", 20, 40); // 20 + 10 = 30
        let out = allocate(vec![stale, fresh], 40);
        assert_eq!(out.selected.len(), 1);
        assert_eq!(out.selected[0].id, "new");
        let dropped = out.skipped.iter().find(|e| e.id == "old").unwrap();
        assert_eq!(dropped.signals.stale_penalty, -STALE_PENALTY);
    }

    #[test]
    fn repeated_files_are_demoted_by_a_redundancy_penalty() {
        // Two hits from the same path: the second is penalized so it competes
        // worse than a distinct-file hit of equal raw score.
        let mut a = candidate(PackSource::Ingest, "a", 50, 30);
        let mut b = candidate(PackSource::Ingest, "b", 50, 30);
        a.path = Some("same.rs".to_string());
        b.path = Some("same.rs".to_string());
        let other = candidate(PackSource::Ingest, "c", 45, 30); // distinct file
        let signals = rank_all(&[a, b, other]);
        // One of the same-path entries carries a redundancy penalty.
        assert!(signals.iter().any(|s| s.redundancy_penalty < 0));
    }

    #[test]
    fn an_exact_file_match_lifts_an_otherwise_lower_candidate() {
        // The file-match bonus (20 points) flips a near-tie (a 10-point
        // relevance gap here), and only a near-tie — a measurable, bounded
        // effect on the normalized scale.
        let mut named = candidate(PackSource::Ingest, "named", 20, 40); // 40 rel +10 +20 file
        named.file_match = true;
        let other = candidate(PackSource::Ingest, "other", 25, 40); // 50 rel +10
        let out = allocate(vec![other, named], 40);
        assert_eq!(out.selected.len(), 1);
        assert_eq!(out.selected[0].id, "named");
        assert_eq!(out.selected[0].signals.file_match, FILE_MATCH_BONUS);
    }

    #[test]
    fn a_closer_graph_neighbor_outranks_a_farther_one() {
        let mut near = candidate(PackSource::CodeGraph, "near", 10, 40);
        let mut far = candidate(PackSource::CodeGraph, "far", 10, 40);
        near.graph_proximity = 0; // the symbol itself
        far.graph_proximity = 3; // distant neighbor: no proximity bonus
        let out = allocate(vec![far, near], 40);
        assert_eq!(out.selected.len(), 1);
        assert_eq!(out.selected[0].id, "near");
        assert!(out.selected[0].signals.graph_proximity > 0);
    }

    #[test]
    fn manual_pins_take_the_highest_precedence() {
        // A low-relevance manual pin survives a flood of high-score ingest hits
        // because its reserve is filled first.
        let mut candidates = vec![candidate(PackSource::ManualPin, "pin", 1, 20)];
        for i in 0..50 {
            candidates.push(candidate(PackSource::Ingest, &format!("i{i}"), 100, 20));
        }
        let out = allocate(candidates, 150);
        assert!(
            out.selected
                .iter()
                .any(|e| e.source == PackSource::ManualPin),
            "the manual pin must be protected by its reserve"
        );
    }
}
