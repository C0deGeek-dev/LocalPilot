//! Measure the reserved set: queries that played no part in finding the fix.
//!
//! Answers are known by construction — each anchor appears in exactly one active
//! memory — so no labelling pass stands between the corpus and the score.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]
use localpilot_localmind::context_hits;
use std::{collections::BTreeMap, path::PathBuf};
fn main() {
    let mut a = std::env::args().skip(1);
    let root = a.next().map(PathBuf::from).expect("root");
    let qrels = std::fs::read_to_string(a.next().expect("qrels")).expect("qrels");
    let queries = std::fs::read_to_string(a.next().expect("queries")).expect("queries");

    let mut want: BTreeMap<String, String> = BTreeMap::new();
    for line in qrels.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() == 4 && f[3] == "1" {
            want.insert(f[0].to_string(), f[2].to_string());
        }
    }
    let mut hit = 0;
    let mut total = 0;
    let mut ranks = Vec::new();
    for line in queries.lines() {
        let Some((id, text)) = line.split_once('\t') else {
            continue;
        };
        let Some(target) = want.get(id) else { continue };
        total += 1;
        let got: Vec<String> = context_hits(&root, text, None)
            .unwrap_or_default()
            .into_iter()
            .map(|h| h.memory_id)
            .collect();
        match got.iter().position(|m| m == target) {
            Some(p) => {
                hit += 1;
                ranks.push(p + 1);
                println!("  {id} HIT  rank {}", p + 1);
            }
            None => println!("  {id} miss ({} returned)", got.len()),
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let recall = hit as f64 / total.max(1) as f64;
    println!("\nheld-out recall: {hit}/{total} = {recall:.3}");
    if !ranks.is_empty() {
        #[allow(clippy::cast_precision_loss)]
        let mrr: f64 = ranks.iter().map(|r| 1.0 / *r as f64).sum::<f64>() / total as f64;
        println!("held-out MRR: {mrr:.3}");
    }
}
