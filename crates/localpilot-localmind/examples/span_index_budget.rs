//! Build the span index over a real session store and report against budgets.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use localpilot_localmind::SpanStore;
use std::{path::PathBuf, time::Instant};

fn main() {
    let root = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    let sessions = root.join(".localmind").join("sessions");
    let db = sessions.join("spans.sqlite");
    for suffix in ["", "-wal", "-shm"] {
        let mut path = db.clone().into_os_string();
        path.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(path));
    }

    let store = SpanStore::open(&root).expect("open span index");
    let started = Instant::now();
    let report = store.index_sessions(&sessions).expect("index");
    let build = started.elapsed();

    let mut source_bytes = 0_u64;
    for entry in std::fs::read_dir(&sessions).unwrap().filter_map(Result::ok) {
        let transcript = entry.path().join("transcript.redacted.txt");
        if let Ok(meta) = std::fs::metadata(&transcript) {
            source_bytes += meta.len();
        }
    }
    let mut index_bytes = 0_u64;
    for suffix in ["", "-wal", "-shm"] {
        let mut path = db.clone().into_os_string();
        path.push(suffix);
        if let Ok(meta) = std::fs::metadata(PathBuf::from(path)) {
            index_bytes += meta.len();
        }
    }

    println!("{report:?}");
    println!("source        {:.1} MiB", source_bytes as f64 / 1_048_576.0);
    println!(
        "index         {:.1} MiB  ({:.3}x source)",
        index_bytes as f64 / 1_048_576.0,
        index_bytes as f64 / source_bytes.max(1) as f64
    );
    println!("build         {:.2} s", build.as_secs_f64());
    println!("spans         {}", store.span_count().unwrap());

    for query in [
        "embedding endpoint unreachable",
        "release train parity",
        "fts5 contentless index",
        "cargo clippy workspace warnings",
    ] {
        let started = Instant::now();
        let hits = store.search(query, 20).unwrap();
        let sessions_hit: std::collections::BTreeSet<&str> =
            hits.iter().map(|hit| hit.session_id.as_str()).collect();
        println!(
            "query {:<34} {:>5} hits from {:>2} sessions in {:>5.1} ms",
            format!("{query:?}"),
            hits.len(),
            sessions_hit.len(),
            started.elapsed().as_secs_f64() * 1000.0
        );
    }

    let second = Instant::now();
    let again = store.index_sessions(&sessions).expect("re-index");
    println!(
        "re-index      {} unchanged, {} indexed, in {:.2} s",
        again.sessions_unchanged,
        again.sessions_indexed,
        second.elapsed().as_secs_f64()
    );
}
