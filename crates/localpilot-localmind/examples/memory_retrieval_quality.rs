//! Run a frozen judgment set through the shipped memory-injection path.
//!
//! Measures `context_hits` — the function that decides what the model actually
//! receives. Measuring the raw store instead would score a component the model
//! never sees.
//!
//! Deterministic and offline: with no embedding endpoint configured, cosines are
//! `None`, the rerank stage is inert, and the path is pure keyword search. That
//! is the shipped default and the right thing to measure first.
//!
//! The judgment set lives outside this repository (it names real memory ids), so
//! this is an example taking paths rather than a committed test.
//!
//! ```text
//! cargo run --release -p localpilot-localmind --example memory_retrieval_quality -- \
//!     D:/repos/LocalX  D:/repos/LocalX/LocalHub/plans/memoryretrievalquality
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use localpilot_localmind::{context_hits, score_query, summarise, JudgmentSet, QueryClass};
use std::{collections::BTreeMap, path::PathBuf};

/// The rank cut every metric is taken at. Matches what the injection path itself
/// caps at, so the measurement scores what a turn would actually receive.
const CUT: usize = 8;

fn main() {
    let mut args = std::env::args().skip(1);
    let project = args.next().map(PathBuf::from).expect("project root");
    let plan_dir = args.next().map(PathBuf::from).expect("plan directory");

    let qrels_path = plan_dir.join("qrels.txt");
    let qrels = std::fs::read_to_string(&qrels_path).expect("qrels.txt");
    let metadata = read_metadata(&plan_dir.join("query-set-draft.md"));
    let set = JudgmentSet::parse(&qrels, &metadata).expect("parse qrels");

    println!("judgment set : {}", qrels_path.display());
    println!("sha256       : {}", set.digest());
    println!("queries      : {}", set.queries().len());
    println!("cut          : top {CUT}\n");

    let mut outcomes = Vec::new();
    let mut unjudged_ids: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for query in set.queries() {
        let hits = context_hits(&project, &query.text, None).expect("context_hits");
        let returned: Vec<String> = hits.into_iter().map(|hit| hit.memory_id).collect();
        if let Some(judged) = &query.judged {
            let judged: std::collections::BTreeSet<&str> =
                judged.iter().map(String::as_str).collect();
            let missing: Vec<String> = returned
                .iter()
                .take(CUT)
                .filter(|id| !judged.contains(id.as_str()))
                .cloned()
                .collect();
            if !missing.is_empty() {
                unjudged_ids.insert(query.id.clone(), missing);
            }
        }
        outcomes.push(score_query(query, &returned, CUT));
    }

    let report = summarise(outcomes, CUT);
    println!("{}", report.render());

    println!("\nper class, with the bounds and coverage D004 requires:\n");
    println!(
        "{:<26} {:>7} {:>10} {:>18} {:>9}",
        "class", "queries", "known-pos", "precision", "coverage"
    );
    println!(
        "{:<26} {:>7} {:>10} {:>18} {:>9}",
        "", "", "recall", "lower..upper", ""
    );
    for (name, summary) in &report.by_class {
        let recall = summary
            .recall
            .map_or_else(|| "   n/a".to_string(), |v| format!("{v:6.2}"));
        let bounds = match (summary.precision_lower, summary.precision_upper) {
            (Some(low), Some(high)) => format!("{low:.2}..{high:.2}"),
            _ => "n/a".to_string(),
        };
        let coverage = summary
            .coverage
            .map_or_else(|| "  n/a".to_string(), |v| format!("{v:5.2}"));
        println!(
            "{:<26} {:>7} {recall:>10} {bounds:>18} {coverage:>9}",
            name, summary.queries
        );
        if let Some(correct) = summary.negatives_correct {
            println!(
                "{:<26} {} of {} correctly returned nothing",
                "", correct, summary.queries
            );
        }
    }

    // The top-up ballot. Anything here is a returned id nobody judged; under
    // D004 it is UNJUDGED, never counted as wrong, and it becomes the next
    // judgment-set revision rather than being scored blind.
    println!();
    if unjudged_ids.is_empty() {
        println!("top-up needed: none — every returned id was judged");
    } else {
        let total: usize = unjudged_ids.values().map(Vec::len).sum();
        println!(
            "top-up needed: {total} unjudged id(s) across {} queries",
            unjudged_ids.len()
        );
        for (query, ids) in &unjudged_ids {
            println!("  {query}: {}", ids.join(", "));
        }
    }
}

/// Read each query's class and prompt from the ballot.
///
/// The qrels file carries judgments only — that is the format's job — so the
/// class a query belongs to comes from the ballot it was labelled on.
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
