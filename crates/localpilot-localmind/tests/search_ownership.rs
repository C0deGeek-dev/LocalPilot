//! Dependency/decision contract for LocalHub#84.
//!
//! The live retrieval behavior is exercised end to end by
//! `learning_loop_round_trip`; this guard pins its architectural owner so the
//! engine crate cannot silently become consumer-free (or acquire a second host
//! implementation) while the accepted decisions still claim one owner.

use std::fs;
use std::path::Path;

#[test]
fn localmind_search_has_exactly_one_host_consumer_and_the_decision_names_it() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let crates = workspace.join("crates");
    let mut consumers = Vec::new();

    for entry in fs::read_dir(&crates).expect("read workspace crates") {
        let entry = entry.expect("read crate entry");
        if !entry.file_type().expect("read crate entry type").is_dir() {
            continue;
        }
        let manifest = entry.path().join("Cargo.toml");
        let Ok(body) = fs::read_to_string(manifest) else {
            continue;
        };
        if body
            .lines()
            .any(|line| line.trim_start().starts_with("localmind-search") && line.contains("path"))
        {
            consumers.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    consumers.sort();

    assert_eq!(
        consumers,
        ["localpilot-localmind"],
        "changing the production-consumer set requires amending ADR-0110 and LocalMind D-LM-0026"
    );

    let decisions = fs::read_to_string(workspace.join("docs/10-decisions.md"))
        .expect("read LocalPilot decisions");
    assert!(
        decisions.contains("localmind-search::hybrid_memory_search")
            && decisions.contains("LocalHub#84"),
        "the dependency edge and its ownership decision must move together"
    );
}
