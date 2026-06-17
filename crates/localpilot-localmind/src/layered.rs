//! Layered, token-budgeted retrieval: index → expand → fetch.
//!
//! Instead of dumping full bodies into context, retrieval is staged so a caller
//! (or the model) spends a few tokens to *locate* the right memory before paying
//! for any body:
//!
//! - **Index** (cheap): id + one-line summary + score for each candidate.
//! - **Expand** (cheap): the document neighbours around chosen ids.
//! - **Fetch** (the only expensive layer): full bodies for explicit ids.
//!
//! Every layer reports its token cost, so the budget being spent is visible. The
//! [`layered_pack`] path lays down cheap index summaries first, then upgrades the
//! top entries to full bodies only while they fit a configurable budget — so a
//! tight budget degrades gracefully to index-only and never overspends.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

use crate::ingest::{self, IngestError};

/// One line, with internal newlines collapsed, capped for an index summary.
const SUMMARY_CHARS: usize = 120;

/// Approximate token estimate: ~4 chars per token, floored at 1 for any
/// non-empty text, 0 for empty.
fn estimate_tokens(text: &str) -> u64 {
    let chars = text.chars().count() as u64;
    if chars == 0 {
        0
    } else {
        (chars / 4).max(1)
    }
}

/// The retrieval layer a result belongs to, cheap → expensive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalLayer {
    Index,
    Expand,
    Fetch,
}

/// Layer 1: a compact pointer to a candidate. No body — just enough to decide
/// whether to expand or fetch it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    pub id: String,
    pub path: String,
    pub summary: String,
    pub score: u64,
    pub stale: bool,
    pub token_cost: u64,
}

/// Layer 2: the document neighbours around a chosen id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expansion {
    pub id: String,
    pub neighbor_ids: Vec<String>,
    pub token_cost: u64,
}

/// Layer 3: a full chunk body for an explicit id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchedBody {
    pub id: String,
    pub path: String,
    pub start_line: u64,
    pub end_line: u64,
    pub body: String,
    pub token_cost: u64,
}

/// A budgeted layered pack: full bodies for the entries that fit, plus index
/// summaries for the rest. `token_estimate` never exceeds `token_budget`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayeredPack {
    pub query: String,
    pub token_budget: u64,
    pub token_estimate: u64,
    pub fetched: Vec<FetchedBody>,
    pub index_only: Vec<IndexEntry>,
    /// True when the budget was too tight to fetch any body, so the pack is
    /// purely index summaries — the graceful-degradation signal.
    pub index_only_degraded: bool,
}

/// Layer 1: the compact index for a query. Ranked, each entry carrying its
/// (small) token cost. Read-only.
///
/// # Errors
/// Returns [`IngestError`] when the derived index cannot be read.
pub fn index_layer(
    project_root: &Path,
    query: &str,
    limit: usize,
) -> Result<Vec<IndexEntry>, IngestError> {
    let hits = ingest::search(project_root, query)?;
    Ok(hits
        .into_iter()
        .take(limit)
        .map(|hit| {
            let summary = one_line(&hit.snippet);
            IndexEntry {
                token_cost: estimate_tokens(&summary).max(1),
                id: hit.chunk_id,
                path: hit.path,
                summary,
                score: hit.score,
                stale: hit.stale,
            }
        })
        .collect())
}

/// Layer 2: document neighbours around each id. Cheap (ids only). Read-only.
///
/// # Errors
/// Returns [`IngestError`] when the derived index cannot be read.
pub fn expand_layer(project_root: &Path, ids: &[String]) -> Result<Vec<Expansion>, IngestError> {
    let mut expansions = Vec::with_capacity(ids.len());
    for id in ids {
        let neighbor_ids = ingest::sibling_chunk_ids(project_root, id)?;
        let token_cost = estimate_tokens(&neighbor_ids.join(" "));
        expansions.push(Expansion {
            id: id.clone(),
            neighbor_ids,
            token_cost,
        });
    }
    Ok(expansions)
}

/// Layer 3: full bodies for an explicit set of ids — and only those ids.
/// Read-only.
///
/// # Errors
/// Returns [`IngestError`] when the derived index cannot be read.
pub fn fetch_layer(project_root: &Path, ids: &[String]) -> Result<Vec<FetchedBody>, IngestError> {
    let chunks = ingest::fetch_chunks(project_root, ids)?;
    Ok(chunks
        .into_iter()
        .map(|chunk| FetchedBody {
            token_cost: chunk.token_estimate.max(estimate_tokens(&chunk.text)),
            id: chunk.id,
            path: chunk.path,
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            body: chunk.text,
        })
        .collect())
}

/// Build a token-bounded layered pack: lay down cheap index summaries up to the
/// budget, then upgrade the highest-ranked entries to full bodies while the
/// delta still fits. The total never exceeds `token_budget`; a budget too tight
/// for any body yields an index-only (degraded) pack. Read-only.
///
/// # Errors
/// Returns [`IngestError`] when the derived index cannot be read.
pub fn layered_pack(
    project_root: &Path,
    query: &str,
    token_budget: u64,
    max_index: usize,
) -> Result<LayeredPack, IngestError> {
    let index = index_layer(project_root, query, max_index)?;
    let had_candidates = !index.is_empty();
    let ids: Vec<String> = index.iter().map(|entry| entry.id.clone()).collect();
    let bodies: HashMap<String, FetchedBody> = fetch_layer(project_root, &ids)?
        .into_iter()
        .map(|body| (body.id.clone(), body))
        .collect();

    let mut used = 0_u64;
    // Phase A: keep index summaries that fit, cheapest contract first. The index
    // is already ranked, so this keeps the most relevant locators.
    let mut kept = Vec::new();
    for entry in index {
        if used.saturating_add(entry.token_cost) <= token_budget {
            used = used.saturating_add(entry.token_cost);
            kept.push(entry);
        }
    }

    // Phase B: upgrade kept entries to full bodies while the extra cost fits.
    let mut fetched = Vec::new();
    let mut index_only = Vec::new();
    for entry in kept {
        match bodies.get(&entry.id) {
            Some(body) => {
                let delta = body.token_cost.saturating_sub(entry.token_cost);
                if used.saturating_add(delta) <= token_budget {
                    used = used.saturating_add(delta);
                    fetched.push(body.clone());
                } else {
                    index_only.push(entry);
                }
            }
            None => index_only.push(entry),
        }
    }

    // Degraded when there were candidates but the budget bought no full body —
    // whether that left index summaries or was too tight for even a locator.
    let index_only_degraded = had_candidates && fetched.is_empty();
    Ok(LayeredPack {
        query: query.to_string(),
        token_budget,
        token_estimate: used,
        fetched,
        index_only,
        index_only_degraded,
    })
}

/// Collapse a snippet to a single capped line for an index summary.
fn one_line(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(SUMMARY_CHARS).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use localpilot_config::IngestConfig;

    /// Ingest a fixture project with two files, one of them large, and return its
    /// root.
    fn ingested_project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        // A large file so its full body dwarfs a one-line index summary.
        let big = format!(
            "// widget module\n{}",
            "fn widget() {} // widget\n".repeat(400)
        );
        std::fs::write(dir.path().join("src/widget.rs"), big).unwrap();
        std::fs::write(
            dir.path().join("src/small.rs"),
            "// gadget helper\nfn gadget() -> u32 { 1 } // gadget\n",
        )
        .unwrap();
        ingest::run(dir.path(), &IngestConfig::default(), ingest::RunMode::Full).unwrap();
        dir
    }

    #[test]
    fn the_index_layer_is_cheaper_per_result_than_a_full_fetch() {
        let dir = ingested_project();
        let index = index_layer(dir.path(), "widget", 5).unwrap();
        assert!(!index.is_empty(), "expected an index hit for widget");
        let ids: Vec<String> = index.iter().map(|entry| entry.id.clone()).collect();
        let bodies = fetch_layer(dir.path(), &ids).unwrap();

        let index_cost: u64 = index.iter().map(|entry| entry.token_cost).sum();
        let fetch_cost: u64 = bodies.iter().map(|body| body.token_cost).sum();
        assert!(
            index_cost * 4 <= fetch_cost,
            "index ({index_cost}) must be materially cheaper than fetch ({fetch_cost})"
        );
    }

    #[test]
    fn fetch_returns_only_the_requested_ids() {
        let dir = ingested_project();
        let index = index_layer(dir.path(), "widget gadget", 10).unwrap();
        assert!(index.len() >= 2, "expected hits across both files");
        let wanted = vec![index[0].id.clone()];

        let bodies = fetch_layer(dir.path(), &wanted).unwrap();
        assert_eq!(bodies.len(), 1, "fetch must return exactly the asked ids");
        assert_eq!(bodies[0].id, wanted[0]);
    }

    #[test]
    fn a_fixed_budget_bounds_the_packaged_tokens() {
        let dir = ingested_project();
        for budget in [10_u64, 50, 200, 5_000] {
            let pack = layered_pack(dir.path(), "widget gadget", budget, 20).unwrap();
            assert!(
                pack.token_estimate <= budget,
                "budget {budget} exceeded: {}",
                pack.token_estimate
            );
        }
    }

    #[test]
    fn a_tight_budget_degrades_to_index_only() {
        let dir = ingested_project();
        // Budget that fits the cheapest summary but never the large body.
        let index = index_layer(dir.path(), "widget", 20).unwrap();
        let summary_cost = index.iter().map(|entry| entry.token_cost).min().unwrap();
        let budget = summary_cost + 1;

        let pack = layered_pack(dir.path(), "widget", budget, 20).unwrap();
        assert!(pack.token_estimate <= budget);
        assert!(
            pack.index_only_degraded,
            "a tight budget must stay index-only: {pack:?}"
        );
        assert!(pack.fetched.is_empty(), "no body should fit a tiny budget");
        assert!(
            !pack.index_only.is_empty(),
            "at least one locator must remain"
        );
    }

    #[test]
    fn a_generous_budget_fetches_full_bodies() {
        let dir = ingested_project();
        let pack = layered_pack(dir.path(), "gadget", 5_000, 20).unwrap();
        assert!(
            !pack.fetched.is_empty(),
            "a large budget should fetch bodies"
        );
        assert!(!pack.index_only_degraded);
    }
}
