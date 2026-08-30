//! Did the kind-scope fix buy retrieval **quality**, or only coverage?
//!
//! The sibling plan measured that scoping the vector scan by subject kind lifted
//! memory candidates per prompt from a median of 1.5 to the full window. That is
//! a coverage number. It says nothing about whether the *right* memories were
//! returned, which is the question this answers.
//!
//! # The two arms
//!
//! **Post-fix** (shipped): scan the vector index **within** `subject_kind =
//! 'memory'`, so the window is spent on memories.
//!
//! **Pre-fix** (`27f613b^`): take a shared top-64 across every subject kind, then
//! filter for memories afterwards. Reconstructed from the code at that commit,
//! not approximated:
//!
//! ```text
//! let scored = persistence.vector_search(&vector, RELEVANCE_VECTOR_WINDOW).ok()?;
//! let memories = scored.into_iter()
//!     .filter(|result| result.subject_kind == "memory")
//!     .map(|result| (result.subject_id, result.score))
//!     .collect();
//! ```
//!
//! Everything downstream — the RRF window, the keyword floor, the cut — is
//! identical between arms, so the only variable is where the dense window is
//! spent.
//!
//! Needs a reachable embedding endpoint: with none, both arms produce no cosines
//! and are trivially identical. That makes this an **opportunistic** measurement
//! under D008, never a blocking gate.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use localpilot_localmind::{
    context_hits, pre_fix_relevance_cosines_for_eval, score_query, summarise, JudgmentSet,
    QueryClass,
};
use std::{collections::BTreeMap, path::PathBuf};

const CUT: usize = 8;

fn main() {
    let mut args = std::env::args().skip(1);
    let project = args.next().map(PathBuf::from).expect("project root");
    let plan_dir = args.next().map(PathBuf::from).expect("plan directory");

    let qrels = std::fs::read_to_string(plan_dir.join("qrels.txt")).expect("qrels.txt");
    let metadata = read_metadata(&plan_dir.join("query-set-draft.md"));
    let set = JudgmentSet::parse(&qrels, &metadata).expect("parse qrels");
    println!("judgment set sha256 : {}", set.digest());
    println!("queries             : {}\n", set.queries().len());

    let mut post = Vec::new();
    let mut pre = Vec::new();
    let mut moved = 0_usize;
    for query in set.queries() {
        let post_ids: Vec<String> = context_hits(&project, &query.text, None)
            .expect("post-fix arm")
            .into_iter()
            .map(|hit| hit.memory_id)
            .collect();
        let pre_ids =
            pre_fix_relevance_cosines_for_eval(&project, &query.text).expect("pre-fix arm");
        if post_ids != pre_ids {
            moved += 1;
        }
        post.push(score_query(query, &post_ids, CUT));
        pre.push(score_query(query, &pre_ids, CUT));
    }

    let post = summarise(post, CUT);
    let pre = summarise(pre, CUT);

    println!(
        "queries whose result list differs between arms: {moved} of {}\n",
        set.queries().len()
    );
    let fmt = |v: Option<f64>| v.map_or_else(|| "n/a".to_string(), |x| format!("{x:.3}"));
    let delta = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(a), Some(b)) => format!("{:+.3}", a - b),
        _ => "n/a".to_string(),
    };
    println!(
        "{:<20} {:>8} {:>8} {:>8}   {:>8} {:>8} {:>8}",
        "class", "pre rec", "post rec", "delta", "pre MRR", "post MRR", "delta"
    );
    for (name, after) in &post.by_class {
        let before = pre.by_class.get(name);
        println!(
            "{:<20} {:>8} {:>8} {:>8}   {:>8} {:>8} {:>8}",
            name,
            fmt(before.and_then(|s| s.recall)),
            fmt(after.recall),
            delta(after.recall, before.and_then(|s| s.recall)),
            fmt(before.map(|s| s.mrr)),
            fmt(Some(after.mrr)),
            delta(Some(after.mrr), before.map(|s| s.mrr)),
        );
    }

    // Per-query, for the ones that actually moved: a class average can hide an
    // improvement and a regression cancelling each other out.
    println!(
        "
queries whose list changed:"
    );
    for (a, b) in post.outcomes.iter().zip(pre.outcomes.iter()) {
        if a.first_relevant_rank != b.first_relevant_rank || a.hit != b.hit {
            println!(
                "  {:<6} {:<18} pre: hit {} rank {:?}   post: hit {} rank {:?}",
                a.id,
                a.class.name(),
                b.hit,
                b.first_relevant_rank,
                a.hit,
                a.first_relevant_rank
            );
        }
    }
    println!(
        "\nnegatives correctly empty — pre: {:?}  post: {:?}",
        pre.by_class
            .get("negative")
            .and_then(|s| s.negatives_correct),
        post.by_class
            .get("negative")
            .and_then(|s| s.negatives_correct)
    );
}

fn read_metadata(ballot: &std::path::Path) -> BTreeMap<String, (QueryClass, String)> {
    let text = std::fs::read_to_string(ballot).expect("ballot");
    let mut out = BTreeMap::new();
    let mut current: Option<(String, QueryClass)> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            current = rest.split_once(" · ").and_then(|(id, class)| {
                let class = match class.trim() {
                    "lexical" => QueryClass::Lexical,
                    "paraphrase" => QueryClass::Paraphrase,
                    "cross-cutting" => QueryClass::CrossSession,
                    "negative-by-construction" => QueryClass::Negative,
                    "near-miss" => QueryClass::NearMiss,
                    _ => return None,
                };
                Some((id.trim().to_string(), class))
            });
        } else if let (Some((id, class)), Some(prompt)) = (&current, line.strip_prefix("> ")) {
            out.entry(id.clone())
                .or_insert_with(|| (*class, prompt.trim().to_string()));
        }
    }
    out
}
