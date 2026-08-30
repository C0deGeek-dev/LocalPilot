//! Is the dense path live, and does cosine separate a real answer from scaffolding junk?
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]
use localmind_store::{MemoryPersistence, MemoryScanStatus, ProjectConfig};
use std::{collections::BTreeMap, path::PathBuf};
fn main() {
    let mut a = std::env::args().skip(1);
    let root = a.next().map(PathBuf::from).expect("root");
    let label = a.next().expect("label");
    let queries = std::fs::read_to_string(a.next().expect("queries")).expect("queries");
    let want: BTreeMap<String, String> = match a.next() {
        Some(path) => std::fs::read_to_string(path)
            .expect("qrels")
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                (f.len() == 4 && f[3] == "1").then(|| (f[0].to_string(), f[2].to_string()))
            })
            .collect(),
        None => BTreeMap::new(),
    };
    // Say which corpus this is measuring, before measuring it. `.cargo/config.toml`
    // sets `LOCALMIND_GLOBAL_ROOT=@project` for hermetic tests and `cargo run
    // --example` inherits it, so an eval silently reads an empty global store.
    let config = ProjectConfig::discover(&root).expect("config");
    eprintln!(
        "corpus: {} | global root {:?}",
        root.display(),
        config.global_memory_root()
    );
    let store = MemoryPersistence::open_project(&root).expect("store");
    let mut cosines: Vec<f32> = Vec::new();
    for line in queries.lines() {
        let Some((id, text)) = line.split_once('\t') else {
            continue;
        };
        let report = match store.memory_vector_scan_diagnosed(text) {
            Ok(r) => r,
            Err(e) => {
                println!("  {id} scan error: {e}");
                continue;
            }
        };
        if !matches!(report.status, MemoryScanStatus::Scanned) {
            println!("  {id} dense path NOT live: {:?}", report.status);
            continue;
        }
        // The junk memory for a negative query, or the known answer for a positive.
        let target = want
            .get(id)
            .cloned()
            .unwrap_or_else(|| "okf-bbb387d37c7b43ad".into());
        match report.scored.iter().find(|r| r.subject_id == target) {
            Some(r) => {
                cosines.push(r.score);
                println!("  {id} {target} cos={:.4}", r.score);
            }
            None => println!("  {id} {target} not in dense scan"),
        }
    }
    cosines.sort_by(|x, y| x.partial_cmp(y).unwrap());
    if !cosines.is_empty() {
        println!(
            "\n{label}: n={} min={:.4} median={:.4} max={:.4}",
            cosines.len(),
            cosines[0],
            cosines[cosines.len() / 2],
            cosines[cosines.len() - 1]
        );
    }
}
