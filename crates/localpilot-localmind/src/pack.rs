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
            PackSource::AcceptedMemory => 0,
            PackSource::RecentSession => 1,
            PackSource::Ingest => 2,
            PackSource::CodeGraph => 3,
        }
    }

    /// Fraction of the total budget reserved for this source. Reserves
    /// deliberately sum to less than one so a shared pool remains for global
    /// competition.
    fn reserve_fraction(self) -> f64 {
        match self {
            PackSource::AcceptedMemory => 0.25,
            PackSource::RecentSession => 0.15,
            PackSource::Ingest => 0.30,
            PackSource::CodeGraph => 0.10,
        }
    }

    /// Every source, for reserve accounting and reporting.
    pub(crate) fn all() -> [PackSource; 4] {
        [
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
    pub score: u64,
    pub token_estimate: u64,
    pub snippet: String,
    pub stale: bool,
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
}

impl PackCandidate {
    fn into_entry(self, reason: String) -> PackEntry {
        PackEntry {
            source: self.source,
            id: self.id,
            path: self.path,
            score: self.score,
            token_estimate: self.token_estimate,
            snippet: self.snippet,
            stale: self.stale,
            reason,
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

/// Compete `candidates` for `budget` tokens. Deterministic: ties break by source
/// priority then id, so the same inputs always produce the same pack.
pub(crate) fn allocate(mut candidates: Vec<PackCandidate>, budget: u64) -> Allocation {
    // Reserve-phase order: source priority, then score desc, then id.
    candidates.sort_by(|a, b| {
        a.source
            .priority()
            .cmp(&b.source.priority())
            .then(b.score.cmp(&a.score))
            .then_with(|| a.id.cmp(&b.id))
    });

    let reserves = reserves(budget);
    let mut allocation = Allocation::default();
    let mut used_total = 0_u64;
    let mut used_by_source: BTreeMap<PackSource, u64> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut selected_ids = BTreeSet::new();
    let mut duplicate_ids = BTreeSet::new();

    // Phase 1: reserves. Fill each source up to its guaranteed share.
    for candidate in &candidates {
        let key = candidate.dedup_key();
        if !seen.insert(key) {
            duplicate_ids.insert(candidate.id.clone());
            allocation.skipped.push(
                candidate
                    .clone()
                    .into_entry("duplicate content".to_string()),
            );
            continue;
        }
        let reserve = reserves.get(&candidate.source).copied().unwrap_or(0);
        let src_used = used_by_source.get(&candidate.source).copied().unwrap_or(0);
        let cost = candidate.token_estimate;
        if src_used.saturating_add(cost) <= reserve && used_total.saturating_add(cost) <= budget {
            used_total = used_total.saturating_add(cost);
            *used_by_source.entry(candidate.source).or_default() += cost;
            selected_ids.insert(candidate.id.clone());
            allocation
                .selected
                .push(candidate.clone().into_entry(format!(
                    "included from {} within reserve",
                    label(candidate.source)
                )));
        }
    }

    // Phase 2: shared pool. Compete leftovers globally by score.
    let mut leftovers: Vec<&PackCandidate> = candidates
        .iter()
        .filter(|candidate| {
            !selected_ids.contains(&candidate.id) && !duplicate_ids.contains(&candidate.id)
        })
        .collect();
    leftovers.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(a.source.priority().cmp(&b.source.priority()))
            .then_with(|| a.id.cmp(&b.id))
    });
    for candidate in leftovers {
        let cost = candidate.token_estimate;
        if used_total.saturating_add(cost) <= budget {
            used_total = used_total.saturating_add(cost);
            *used_by_source.entry(candidate.source).or_default() += cost;
            allocation
                .selected
                .push(candidate.clone().into_entry(format!(
                    "included from {} within shared budget",
                    label(candidate.source)
                )));
        } else {
            allocation
                .skipped
                .push(candidate.clone().into_entry(format!(
                    "skipped: budget exhausted ({cost} tokens did not fit)"
                )));
        }
    }

    allocation.token_estimate = used_total;
    allocation.per_source_tokens = used_by_source;
    allocation
}

fn label(source: PackSource) -> &'static str {
    match source {
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
            token_estimate: tokens,
            snippet: format!("snippet {id}"),
            stale: false,
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
        // Many cheap ingest hits plus one accepted-memory anchor; the anchor
        // must survive on its reserve even though ingest has far more hits.
        let mut candidates = vec![candidate(PackSource::AcceptedMemory, "anchor", 1, 20)];
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
}
