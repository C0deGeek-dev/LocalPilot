//! Reciprocal Rank Fusion (RRF).
//!
//! Fuses several ranked lists of the same items into one ranking by summing a
//! rank-based reciprocal score across the lists, so an item that ranks well in
//! more than one retriever outranks an item that tops a single one. This is how
//! the memory retrieval blends its lexical (BM25) ranking with the optional
//! dense (cosine) ranking without either dominating.
//!
//! RRF is a published information-retrieval method (Cormack, Clarke & Büttcher,
//! 2009); the implementation here is original.

use std::collections::HashMap;

/// The RRF damping constant. The published default; larger values flatten the
/// contribution of a high rank, smaller values sharpen it.
pub const RRF_K: f64 = 60.0;

/// Fuse `ranked_lists` (each best-first) into one ranking.
///
/// Each list contributes `1 / (k + rank)` to an item's score, where `rank` is
/// the item's 1-based position in that list. Items are returned best-first;
/// ties break deterministically by id ascending so the fusion is stable.
///
/// A single input list is returned in its original order (RRF of one list is
/// order-preserving), which is the no-op the caller relies on when only the
/// lexical retriever is present.
#[must_use]
pub fn reciprocal_rank_fusion(ranked_lists: &[Vec<String>], k: f64) -> Vec<(String, f64)> {
    let mut scores: HashMap<&str, f64> = HashMap::new();
    for list in ranked_lists {
        for (index, id) in list.iter().enumerate() {
            // rank is 1-based.
            let rank = index as f64 + 1.0;
            *scores.entry(id.as_str()).or_insert(0.0) += 1.0 / (k + rank);
        }
    }
    let mut fused: Vec<(String, f64)> = scores
        .into_iter()
        .map(|(id, score)| (id.to_string(), score))
        .collect();
    // Best score first; deterministic id tiebreak keeps the order stable across
    // runs (HashMap iteration order is not).
    fused.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    fused
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    fn order(ranked_lists: &[Vec<String>]) -> Vec<String> {
        reciprocal_rank_fusion(ranked_lists, RRF_K)
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    #[test]
    fn an_item_in_both_lists_beats_one_in_a_single_list() {
        // The core RRF property: `y` is retrieved by both retrievers, while `x`
        // (lexical only) and `z` (dense only) each appear once — even at rank 1.
        // Cross-retriever agreement wins.
        let lexical = ids(&["x", "y"]);
        let dense = ids(&["y", "z"]);
        assert_eq!(
            order(&[lexical, dense])[0],
            "y",
            "an item found by both retrievers fuses to the top"
        );
    }

    #[test]
    fn score_matches_the_formula() {
        // A doc at rank 1 in both lists scores 1/(60+1) twice.
        let a = ids(&["d", "e"]);
        let b = ids(&["d", "f"]);
        let fused = reciprocal_rank_fusion(&[a, b], RRF_K);
        let d = fused.iter().find(|(id, _)| id == "d").unwrap().1;
        assert!((d - (2.0 / 61.0)).abs() < 1e-12, "d scored {d}");
    }

    #[test]
    fn a_single_list_is_order_preserving() {
        // RRF of one list must not reorder it — the no-regression guarantee when
        // only the lexical retriever is present.
        let only = ids(&["a", "b", "c", "d"]);
        assert_eq!(order(&[only.clone()]), only);
    }

    #[test]
    fn ties_break_deterministically_by_id() {
        // Two docs each rank 1 once and rank 2 once → equal score → id order.
        let a = ids(&["m", "n"]);
        let b = ids(&["n", "m"]);
        assert_eq!(order(&[a, b]), ids(&["m", "n"]));
    }
}
