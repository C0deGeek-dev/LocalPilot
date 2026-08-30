//! Locators, and the promise that one never resolves to the wrong thing.
//!
//! The span index stores no text, so a locator is the *only* way to get a span
//! back. That makes the fetch path load-bearing: every failure mode here is a
//! way a locator could quietly answer with text that is not what was indexed.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use localpilot_localmind::{parse_span_locator, span_locator, SpanMiss, SpanStore};
use std::path::{Path, PathBuf};

fn session(root: &Path, id: &str, turns: &[(&str, &str)]) -> PathBuf {
    let directory = root.join(".localmind").join("sessions").join(id);
    std::fs::create_dir_all(&directory).unwrap();
    let mut body = String::new();
    for (role, text) in turns {
        body.push_str(&format!(
            r#"{{"type":"{role}","message":{{"role":"{role}","content":{}}}}}"#,
            serde_json::to_string(text).unwrap()
        ));
        body.push('\n');
    }
    let transcript = directory.join("transcript.redacted.txt");
    std::fs::write(&transcript, body).unwrap();
    transcript
}

fn sessions_dir(root: &Path) -> PathBuf {
    root.join(".localmind").join("sessions")
}

#[test]
fn a_locator_round_trips_through_parsing() {
    for (session, version, ordinal) in [
        ("session-249da0e17485266f", 1_u32, 0_usize),
        ("session-with-many-hyphens", 7, 12345),
    ] {
        let id = format!("span:{session}:{version}:{ordinal}");
        assert_eq!(
            parse_span_locator(&id),
            Some((session.to_string(), version, ordinal))
        );
    }
}

#[test]
fn an_id_from_another_source_is_not_a_span_locator() {
    // The fetch path partitions on this, so a false positive would send an
    // ingest chunk id down the span route and report it as missing.
    for id in [
        "memory:abc",
        "graph:sym",
        "session:3",
        "chunk-1",
        "span:",
        "span:only-one-part",
        "span:session:notanumber:0",
        "",
    ] {
        assert_eq!(parse_span_locator(id), None, "{id:?} must not parse");
    }
}

#[test]
fn a_span_found_by_search_is_fetched_back_byte_identical() {
    // The round trip the whole design rests on: indexed, found, fetched, equal.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let text = "the release train needs every repo at parity before the tag";
    session(root, "session-a", &[("user", text)]);
    let store = SpanStore::open(root).unwrap();
    store.index_sessions(&sessions_dir(root)).unwrap();

    let hits = store.search("release train parity", 5).unwrap();
    assert_eq!(hits.len(), 1);
    let locator = span_locator(&hits[0]);
    let fetched = store.fetch_span(&locator).unwrap().expect("resolvable");

    assert_eq!(fetched.text, text, "a fetched span must equal its source");
    assert_eq!(fetched.locator, locator);
    assert_eq!(fetched.session_id, "session-a");
    assert!(fetched.start_line >= 1);
}

#[test]
fn a_locator_from_an_older_chunking_contract_resolves_to_nothing() {
    // Re-chunking renumbers ordinals. Answering an old locator with whatever now
    // occupies that slot would be a wrong answer wearing a correct-looking id.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    session(root, "session-a", &[("user", "content")]);
    let store = SpanStore::open(root).unwrap();
    store.index_sessions(&sessions_dir(root)).unwrap();

    let stale = "span:session-a:999:0";
    assert_eq!(
        store.fetch_span(stale).unwrap(),
        Err(SpanMiss::StaleContract)
    );
}

#[test]
fn a_locator_into_a_deleted_session_resolves_to_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    session(root, "session-a", &[("user", "ephemeral")]);
    let store = SpanStore::open(root).unwrap();
    store.index_sessions(&sessions_dir(root)).unwrap();
    let locator = span_locator(&store.search("ephemeral", 5).unwrap()[0]);
    assert!(store.fetch_span(&locator).unwrap().is_ok());

    std::fs::remove_dir_all(sessions_dir(root).join("session-a")).unwrap();
    assert_eq!(store.fetch_span(&locator).unwrap(), Err(SpanMiss::Gone));
}

#[test]
fn a_transcript_edited_after_indexing_is_refused_rather_than_answered() {
    // The failure mode a contentless index makes possible: the ordinal still
    // exists, but the text at it is no longer the text that was indexed. The
    // stored hash is the only place this can be caught.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    session(root, "session-a", &[("user", "the original wording")]);
    let store = SpanStore::open(root).unwrap();
    store.index_sessions(&sessions_dir(root)).unwrap();
    let locator = span_locator(&store.search("original wording", 5).unwrap()[0]);

    // Same shape, same ordinal, different content — and the index is not told.
    session(
        root,
        "session-a",
        &[("user", "a completely different sentence")],
    );

    assert_eq!(store.fetch_span(&locator).unwrap(), Err(SpanMiss::Changed));
}

#[test]
fn every_locator_a_search_emits_can_be_fetched() {
    // The dead-pointer prohibition: no path may emit a locator the fetch path
    // cannot resolve. If a later change adds stub-on-evict, this is the test
    // that stops it shipping half-built.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    session(
        root,
        "session-a",
        &[
            ("user", "how does the embedding lifecycle work"),
            (
                "assistant",
                "the embedder starts with the session and stops with it",
            ),
        ],
    );
    session(
        root,
        "session-b",
        &[("user", "embedding endpoint refused a remote host")],
    );
    let store = SpanStore::open(root).unwrap();
    store.index_sessions(&sessions_dir(root)).unwrap();

    let hits = store.search("embedding", 50).unwrap();
    let sessions: std::collections::BTreeSet<&str> =
        hits.iter().map(|hit| hit.session_id.as_str()).collect();
    assert_eq!(
        sessions.len(),
        2,
        "the point of the index is reach: {hits:?}"
    );
    for hit in &hits {
        let locator = span_locator(hit);
        let fetched = store
            .fetch_span(&locator)
            .unwrap()
            .unwrap_or_else(|miss| panic!("{locator} was emitted but does not resolve: {miss:?}"));
        assert!(!fetched.text.is_empty(), "{locator} resolved to nothing");
    }
}

#[test]
fn many_hits_from_one_session_resolve_in_a_single_pass() {
    // Resolution is grouped by session because the index holds no text: doing it
    // per hit re-reads and re-chunks the transcript once per result.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let turns: Vec<(&str, String)> = (0..40)
        .map(|index| ("user", format!("recurring keyword turn number {index}")))
        .collect();
    let borrowed: Vec<(&str, &str)> = turns
        .iter()
        .map(|(role, text)| (*role, text.as_str()))
        .collect();
    session(root, "session-a", &borrowed);
    let store = SpanStore::open(root).unwrap();
    store.index_sessions(&sessions_dir(root)).unwrap();

    let hits = store.search("recurring keyword", 40).unwrap();
    assert_eq!(hits.len(), 40);
    let texts = store.span_texts(&hits);
    assert_eq!(texts.len(), 40);
    assert!(
        texts.iter().all(Option::is_some),
        "every hit must resolve to its text"
    );
}

#[test]
fn searching_a_project_without_a_span_index_creates_nothing() {
    // `SpanStore::open` creates the database, and the pack builder runs on an
    // ordinary prompt. Without an existence check first, every search would
    // leave an empty database behind — and "a plain prompt never creates
    // project files" is a rule this repo already made once.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(sessions_dir(root)).unwrap();
    assert!(!localpilot_localmind::has_span_index(root));

    // Whatever the pack builder does for a query, it must not be this.
    let _ = localpilot_localmind::compute_pack(root, "any query", 4096, None);

    assert!(
        !localpilot_localmind::has_span_index(root),
        "a read path must not create the index it reads"
    );
}
