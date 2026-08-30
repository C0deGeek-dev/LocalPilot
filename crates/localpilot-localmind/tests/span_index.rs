//! The searchable index over session transcript spans.
//!
//! The reach is the point: `recent_session_facts` already covers "newest
//! session, six key points". These tests are mostly about the properties that
//! make an index over *every* session safe to keep — idempotence, deletion
//! propagation, and refusing to write outside the project.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use localpilot_localmind::{SpanKind, SpanStore, SpanStoreError};
use std::path::{Path, PathBuf};

fn session(root: &Path, id: &str, body: &str) -> PathBuf {
    let directory = root.join(".localmind").join("sessions").join(id);
    std::fs::create_dir_all(&directory).unwrap();
    let transcript = directory.join("transcript.redacted.txt");
    std::fs::write(&transcript, body).unwrap();
    transcript
}

fn sessions_dir(root: &Path) -> PathBuf {
    root.join(".localmind").join("sessions")
}

fn claude(lines: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (role, text) in lines {
        out.push_str(&format!(
            r#"{{"type":"{role}","message":{{"role":"{role}","content":{}}}}}"#,
            serde_json::to_string(text).unwrap()
        ));
        out.push('\n');
    }
    out
}

#[test]
fn spans_from_many_sessions_are_searchable_together() {
    // The actual gap: the existing session source reads one session's summary.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    session(
        root,
        "session-a",
        &claude(&[("user", "how do I restart the embedding server")]),
    );
    session(
        root,
        "session-b",
        &claude(&[("assistant", "the embedding server listens on loopback only")]),
    );
    let store = SpanStore::open(root).unwrap();
    let report = store.index_sessions(&sessions_dir(root)).unwrap();
    assert_eq!(report.sessions_indexed, 2);
    assert_eq!(report.spans_written, 2);

    let hits = store.search("embedding server", 10).unwrap();
    let found: std::collections::BTreeSet<&str> =
        hits.iter().map(|hit| hit.session_id.as_str()).collect();
    assert_eq!(
        found,
        ["session-a", "session-b"].into_iter().collect(),
        "a query must reach across sessions, which is the whole point"
    );
}

#[test]
fn re_indexing_an_unchanged_corpus_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    session(root, "session-a", &claude(&[("user", "unchanged content")]));
    let store = SpanStore::open(root).unwrap();

    let first = store.index_sessions(&sessions_dir(root)).unwrap();
    assert_eq!(first.sessions_indexed, 1);

    let second = store.index_sessions(&sessions_dir(root)).unwrap();
    assert_eq!(second.sessions_indexed, 0);
    assert_eq!(second.sessions_unchanged, 1);
    assert_eq!(second.spans_written, 0);
}

#[test]
fn indexing_is_idempotent_rather_than_accumulating() {
    // Idempotence matters more than speed here: it is what makes the lifecycle
    // work tractable, and a store that grows on every pass is one that silently
    // returns each hit twice, then three times.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    session(root, "session-a", &claude(&[("user", "stable text")]));
    let store = SpanStore::open(root).unwrap();

    store.index_sessions(&sessions_dir(root)).unwrap();
    let after_first = store.span_count().unwrap();
    // Force a re-index of identical content by clearing the change record's
    // effect: rewrite the file with the same bytes and a different session.
    session(root, "session-b", &claude(&[("user", "stable text")]));
    store.index_sessions(&sessions_dir(root)).unwrap();
    store.index_sessions(&sessions_dir(root)).unwrap();
    store.index_sessions(&sessions_dir(root)).unwrap();

    assert_eq!(store.span_count().unwrap(), after_first * 2);
    assert_eq!(store.search("stable text", 10).unwrap().len(), 2);
}

#[test]
fn a_grown_session_is_re_indexed_without_a_full_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    session(root, "session-a", &claude(&[("user", "first turn")]));
    session(root, "session-b", &claude(&[("user", "untouched")]));
    let store = SpanStore::open(root).unwrap();
    store.index_sessions(&sessions_dir(root)).unwrap();

    session(
        root,
        "session-a",
        &claude(&[("user", "first turn"), ("assistant", "second turn")]),
    );
    let report = store.index_sessions(&sessions_dir(root)).unwrap();
    assert_eq!(report.sessions_indexed, 1, "only the grown session");
    assert_eq!(report.sessions_unchanged, 1);

    // Terms are OR'd, so "second turn" also reaches the span saying "first
    // turn" — recall is the query's job and ordering is bm25's. What matters is
    // that the new span exists and ranks first.
    let hits = store.search("second turn", 10).unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0].session_id, "session-a");
    assert_eq!(hits[0].kind, SpanKind::AssistantMessage);
}

#[test]
fn deleting_a_session_takes_its_spans_with_it() {
    // The index never outlives its source. Without this it becomes a durable
    // copy of material someone deleted.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    session(
        root,
        "session-a",
        &claude(&[("user", "ephemeral secret plan")]),
    );
    session(root, "session-b", &claude(&[("user", "kept")]));
    let store = SpanStore::open(root).unwrap();
    store.index_sessions(&sessions_dir(root)).unwrap();
    assert_eq!(store.search("ephemeral", 10).unwrap().len(), 1);

    std::fs::remove_dir_all(sessions_dir(root).join("session-a")).unwrap();
    let report = store.index_sessions(&sessions_dir(root)).unwrap();

    assert_eq!(report.sessions_removed, 1);
    assert!(
        store.search("ephemeral", 10).unwrap().is_empty(),
        "a deleted session's spans must be gone from the index too"
    );
    assert_eq!(store.session_count().unwrap(), 1);
    assert_eq!(store.search("kept", 10).unwrap().len(), 1);
}

#[test]
fn an_index_outside_the_project_is_refused() {
    // Inherited privacy clause: the index derives from session transcripts and
    // may not be written outside the project that produced them.
    let project = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let error = SpanStore::open_at(project.path(), elsewhere.path())
        .expect_err("an index outside the project must be refused");
    assert!(matches!(error, SpanStoreError::OutsideProject { .. }));

    // And it must not be fooled by a path that only looks like a child.
    let escape = project.path().join(".localmind").join("..").join("..");
    assert!(matches!(
        SpanStore::open_at(project.path(), &escape),
        Err(SpanStoreError::OutsideProject { .. })
    ));
}

#[test]
fn an_index_inside_the_project_is_permitted() {
    let project = tempfile::tempdir().unwrap();
    let inside = project.path().join(".localmind").join("sessions");
    assert!(SpanStore::open_at(project.path(), &inside).is_ok());
}

#[test]
fn a_store_from_a_newer_build_is_refused_rather_than_migrated() {
    // An older binary meeting a newer store must fail clearly. The index is
    // derived, so refusing costs a rebuild; guessing costs correctness.
    let project = tempfile::tempdir().unwrap();
    let directory = project.path().join(".localmind").join("sessions");
    std::fs::create_dir_all(&directory).unwrap();
    {
        let connection = rusqlite::Connection::open(directory.join("spans.sqlite")).unwrap();
        connection
            .execute_batch("PRAGMA user_version = 9999;")
            .unwrap();
    }
    let error =
        SpanStore::open(project.path()).expect_err("a store from the future must not be opened");
    match error {
        SpanStoreError::SchemaTooNew { found, .. } => assert_eq!(found, 9999),
        other => panic!("expected a schema refusal, got {other:?}"),
    }
    // The message has to say what to do about it.
    assert!(SpanStore::open(project.path())
        .unwrap_err()
        .to_string()
        .contains("re-indexing"));
}

#[test]
fn a_reopened_store_keeps_its_spans() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    session(
        root,
        "session-a",
        &claude(&[("user", "durable across reopen")]),
    );
    {
        let store = SpanStore::open(root).unwrap();
        store.index_sessions(&sessions_dir(root)).unwrap();
    }
    let reopened = SpanStore::open(root).unwrap();
    assert_eq!(reopened.search("durable", 10).unwrap().len(), 1);
    // And a second pass still recognises it as unchanged.
    let report = reopened.index_sessions(&sessions_dir(root)).unwrap();
    assert_eq!(report.sessions_unchanged, 1);
}

#[test]
fn recovery_telemetry_survives_the_run_that_produced_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut body = claude(&[("user", "good record")]);
    body.push_str("{\"type\":\"user\",\"message\":{ TRUNCATED\n");
    session(root, "session-a", &body);
    let store = SpanStore::open(root).unwrap();
    let report = store.index_sessions(&sessions_dir(root)).unwrap();
    assert_eq!(report.unparseable_lines, 1);
    assert_eq!(report.spans_written, 1, "one bad record costs one record");
}

#[test]
fn hits_carry_a_locator_and_never_text() {
    // The index is contentless: it can return where a span is, never what it
    // says. That is what makes the fetch path load-bearing rather than optional.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    session(
        root,
        "session-a",
        &claude(&[("user", "locate this distinctive phrase")]),
    );
    let store = SpanStore::open(root).unwrap();
    store.index_sessions(&sessions_dir(root)).unwrap();

    let hits = store.search("distinctive", 10).unwrap();
    assert_eq!(hits.len(), 1);
    let hit = &hits[0];
    assert_eq!(hit.session_id, "session-a");
    assert_eq!(hit.kind, SpanKind::UserMessage);
    assert_eq!(
        hit.chunking_version,
        localpilot_localmind::SPAN_CHUNKING_VERSION
    );
    assert!(hit.start_line >= 1);
}

#[test]
fn a_query_fts5_cannot_parse_returns_nothing_rather_than_failing() {
    // A model writes prose, and prose contains characters FTS5 reads as
    // operators. An unusable query is an empty result, not an error upward.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    session(root, "session-a", &claude(&[("user", "ordinary content")]));
    let store = SpanStore::open(root).unwrap();
    store.index_sessions(&sessions_dir(root)).unwrap();

    for query in ["\"", "* AND", "()", "   ", "-"] {
        assert!(
            store.search(query, 10).is_ok(),
            "{query:?} must not become an error"
        );
    }
    assert_eq!(store.search("ordinary", 10).unwrap().len(), 1);
}

#[test]
fn an_empty_corpus_indexes_to_nothing_without_complaint() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(sessions_dir(root)).unwrap();
    let store = SpanStore::open(root).unwrap();
    let report = store.index_sessions(&sessions_dir(root)).unwrap();
    assert_eq!(report.sessions_seen, 0);
    assert_eq!(store.span_count().unwrap(), 0);
}
