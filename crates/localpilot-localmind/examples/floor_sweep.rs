//! What the relevance floor costs and buys, per class, at several values.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]
use localmind_store::ProjectConfig;
use localpilot_localmind::context_hits;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

fn main() {
    let mut a = std::env::args().skip(1);
    let root = a.next().map(PathBuf::from).expect("root");
    let queries = std::fs::read_to_string(a.next().expect("queries")).expect("queries");
    let qrels = a.next().and_then(|p| std::fs::read_to_string(p).ok());
    let classes = a.next().and_then(|p| std::fs::read_to_string(p).ok());

    // Say which corpus this is measuring, before measuring it.
    //
    // `.cargo/config.toml` sets `LOCALMIND_GLOBAL_ROOT=@project` so tests are
    // hermetic, and `cargo run --example` inherits it. An eval run therefore
    // reads an empty global store unless the variable is overridden, and reports
    // a number for a corpus a third the size of the real one without saying so.
    // It cost a day of chasing a recall figure that had collapsed for that reason
    // alone. One line of provenance is the whole fix.
    let config = ProjectConfig::discover(&root).expect("config");
    eprintln!(
        "corpus: {} | global root {:?}",
        root.display(),
        config.global_memory_root()
    );

    let mut want: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for line in qrels.iter().flat_map(|q| q.lines()) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() == 4 && f[3] == "1" {
            want.entry(f[0].to_string())
                .or_default()
                .insert(f[2].to_string());
        }
    }
    let class_of: BTreeMap<String, String> = classes
        .iter()
        .flat_map(|c| c.lines())
        .filter_map(|l| l.split_once('\t'))
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();

    // (query, hits) once; the floor is then applied offline so every value is
    // measured against exactly the same retrieval.
    let mut rows = Vec::new();
    for line in queries.lines() {
        let Some((id, text)) = line.split_once('\t') else {
            continue;
        };
        rows.push((
            id.to_string(),
            context_hits(&root, text, None).unwrap_or_default(),
        ));
    }

    for floor in [0.0_f32, 0.30, 0.36, 0.40, 0.60] {
        let mut per_class: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        let mut noisy = 0;
        let mut negatives = 0;
        for (id, hits) in &rows {
            let kept: Vec<_> = hits
                .iter()
                .filter(|h| {
                    floor <= 0.0 || h.subject_matched || !h.cosine.is_some_and(|c| c < floor)
                })
                .collect();
            match want.get(id) {
                Some(targets) => {
                    let class = class_of.get(id).cloned().unwrap_or_else(|| "all".into());
                    let e = per_class.entry(class).or_default();
                    e.1 += 1;
                    if kept.iter().any(|h| targets.contains(&h.memory_id)) {
                        e.0 += 1;
                    }
                }
                None => {
                    negatives += 1;
                    if !kept.is_empty() {
                        noisy += 1;
                    }
                }
            }
        }
        print!("floor {floor:.2} |");
        for (class, (hit, total)) in &per_class {
            #[allow(clippy::cast_precision_loss)]
            let r = *hit as f64 / (*total).max(1) as f64;
            print!(" {class} {r:.2} ({hit}/{total})");
        }
        if negatives > 0 {
            print!(" | over-retrieval {noisy}/{negatives}");
        }
        println!();
    }
}
