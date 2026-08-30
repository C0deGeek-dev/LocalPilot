//! What does the term-coverage gate actually buy?
//!
//! Its cost is measured and specific: it drops the memory holding an exact
//! identifier while keeping one that matches two ordinary English words from the
//! query's own phrasing. Its benefit has never been measured at all.
//!
//! That asymmetry is the reason this runs before anything is changed. Removing a
//! guard because its failure mode is visible and its success is not is how the
//! next defect gets written.
//!
//! Two arms over the frozen judgment set: the **shipped** gate against a
//! **relaxed** one that never fires. Everything downstream — the dense cosines,
//! the fusion window, the cut — is identical, so the only variable is the gate.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use localmind_store::CoverageGate;
use localpilot_localmind::{
    context_hits_gated_for_eval, score_query, summarise, JudgmentSet, QueryClass,
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

    let mut shipped = Vec::new();
    let mut relaxed = Vec::new();
    let mut admitted_total = 0_usize;
    let mut admitted_relevant = 0_usize;
    let mut helped = Vec::new();
    let mut hurt = Vec::new();

    for query in set.queries() {
        let a = run(&project, &query.text, CoverageGate::shipped());
        let b = run(&project, &query.text, CoverageGate::disabled());

        // What relaxing the gate lets in, and whether any of it was worth having.
        let extra: Vec<&String> = b.iter().filter(|id| !a.contains(id)).collect();
        admitted_total += extra.len();
        let relevant: std::collections::BTreeSet<&str> =
            query.relevant.iter().map(String::as_str).collect();
        let extra_relevant = extra
            .iter()
            .filter(|id| relevant.contains(id.as_str()))
            .count();
        admitted_relevant += extra_relevant;

        let sa = score_query(query, &a, CUT);
        let sb = score_query(query, &b, CUT);
        // Where the gate helps: relaxing it made this query worse.
        match (sa.recall(), sb.recall()) {
            (Some(x), Some(y)) if y < x => hurt.push((query.id.clone(), x, y)),
            (Some(x), Some(y)) if y > x => helped.push((query.id.clone(), x, y)),
            _ => {}
        }
        // A negative query the gate was keeping quiet is the clearest case of the
        // gate earning its place.
        if query.relevant.is_empty() && a.is_empty() && !b.is_empty() {
            hurt.push((format!("{} (negative)", query.id), 1.0, 0.0));
        }
        shipped.push(sa);
        relaxed.push(sb);
    }

    let shipped = summarise(shipped, CUT);
    let relaxed = summarise(relaxed, CUT);

    let fmt = |v: Option<f64>| v.map_or_else(|| "n/a".to_string(), |x| format!("{x:.3}"));
    println!(
        "{:<20} {:>10} {:>10} {:>9}   {:>10} {:>10}",
        "class", "shipped", "relaxed", "delta", "ship prec", "relax prec"
    );
    for (name, ship) in &shipped.by_class {
        let rel = relaxed.by_class.get(name);
        let delta = match (rel.and_then(|s| s.recall), ship.recall) {
            (Some(r), Some(s)) => format!("{:+.3}", r - s),
            _ => "n/a".to_string(),
        };
        println!(
            "{:<20} {:>10} {:>10} {:>9}   {:>10} {:>10}",
            name,
            fmt(ship.recall),
            fmt(rel.and_then(|s| s.recall)),
            delta,
            fmt(ship.precision_lower),
            fmt(rel.and_then(|s| s.precision_lower)),
        );
    }

    println!("\nwhat relaxing the gate admits:");
    println!("  {admitted_total} extra result(s) across all queries");
    println!("  {admitted_relevant} of them judged RELEVANT");
    println!(
        "  {} judged irrelevant or unjudged",
        admitted_total - admitted_relevant
    );

    println!(
        "\nqueries the gate HELPS (relaxing made them worse): {}",
        hurt.len()
    );
    for (id, before, after) in &hurt {
        println!("  {id}: recall {before:.2} -> {after:.2}");
    }
    println!(
        "queries the gate HURTS (relaxing made them better): {}",
        helped.len()
    );
    for (id, before, after) in &helped {
        println!("  {id}: recall {before:.2} -> {after:.2}");
    }
}

fn run(project: &std::path::Path, query: &str, gate: CoverageGate) -> Vec<String> {
    context_hits_gated_for_eval(project, query, gate)
        .unwrap_or_default()
        .into_iter()
        .map(|hit| hit.memory_id)
        .collect()
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
