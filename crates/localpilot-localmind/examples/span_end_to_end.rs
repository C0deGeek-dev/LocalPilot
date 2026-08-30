//! Drive search → locator → fetch against a real session store.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use localpilot_localmind::{span_locator, SpanStore};
use std::path::PathBuf;

fn main() {
    let root = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    let sessions = root.join(".localmind").join("sessions");
    let store = SpanStore::open(&root).expect("open");
    let report = store.index_sessions(&sessions).expect("index");
    println!(
        "indexed {} sessions ({} unchanged), {} spans\n",
        report.sessions_indexed, report.sessions_unchanged, report.spans_written
    );

    for query in [
        "embedding endpoint loopback refused",
        "release train parity gitlink",
        "contentless fts5 index budget",
    ] {
        let hits = store.search(query, 6).unwrap();
        let distinct: std::collections::BTreeSet<&str> =
            hits.iter().map(|h| h.session_id.as_str()).collect();
        println!(
            "query {query:?} -> {} hits from {} sessions",
            hits.len(),
            distinct.len()
        );
        let mut resolved = 0;
        let mut failed = 0;
        for hit in &hits {
            let locator = span_locator(hit);
            match store.fetch_span(&locator).unwrap() {
                Ok(span) => {
                    resolved += 1;
                    if resolved == 1 {
                        let preview: String = span.text.chars().take(110).collect();
                        println!(
                            "   first hit {} [{:?}] lines {}-{}\n     {}",
                            span.locator,
                            span.kind,
                            span.start_line,
                            span.end_line,
                            preview.replace('\n', " ")
                        );
                    }
                }
                Err(miss) => {
                    failed += 1;
                    println!("   UNRESOLVABLE {locator}: {miss:?}");
                }
            }
        }
        println!(
            "   resolved {resolved}/{}, unresolvable {failed}\n",
            hits.len()
        );
    }
}
