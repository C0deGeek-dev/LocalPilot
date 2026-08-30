//! What happens to the index as sessions arrive, grow, change, and go.
//!
//! Three of these are privacy-critical in the same way, and each is written as a
//! scenario rather than an assertion on a helper: **source replacement**,
//! **redaction change**, and **deletion**. All three strand private content in a
//! searchable index if they silently no-op, and a unit test on the helper that
//! *would* have removed it proves nothing about whether the pass calls it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use localpilot_localmind::{span_locator, SpanMiss, SpanStore};
use std::path::{Path, PathBuf};

fn write_session(root: &Path, id: &str, turns: &[(&str, &str)]) {
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
    std::fs::write(directory.join("transcript.redacted.txt"), body).unwrap();
}

fn sessions_dir(root: &Path) -> PathBuf {
    root.join(".localmind").join("sessions")
}

fn found(store: &SpanStore, term: &str) -> usize {
    store.search(term, 50).unwrap().len()
}

// ---------------------------------------------------------------- privacy

#[test]
fn a_transcript_rewritten_in_place_loses_its_old_spans() {
    // Source replacement. The file is not deleted, so nothing announces the
    // change — only its content hash differs. If that goes unnoticed, the old
    // text stays searchable while the transcript no longer contains it, which is
    // the worst shape this failure can take: material the user believes is gone.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_session(root, "session-a", &[("user", "swordfish passphrase")]);
    let store = SpanStore::open(root).unwrap();
    store.index_sessions(&sessions_dir(root)).unwrap();
    assert_eq!(found(&store, "swordfish"), 1);

    write_session(root, "session-a", &[("user", "an ordinary sentence")]);
    let report = store.index_sessions(&sessions_dir(root)).unwrap();

    assert_eq!(report.sessions_indexed, 1, "the rewrite must be noticed");
    assert_eq!(
        found(&store, "swordfish"),
        0,
        "superseded spans must leave the index, not be orphaned in it"
    );
    assert_eq!(found(&store, "ordinary"), 1);
}

#[test]
fn re_redacting_a_transcript_removes_what_the_new_rules_hide() {
    // Redaction-rule change. Rules do not live in the index — they produce the
    // transcript — so a rules change reaches the index only through a rewritten
    // transcript. That makes this the same mechanism as source replacement, and
    // it is pinned separately because the *consequence* is different: here the
    // point is that content the new rules hide can no longer be searched.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_session(
        root,
        "session-a",
        &[("user", "connect with token hunter2 to the server")],
    );
    let store = SpanStore::open(root).unwrap();
    store.index_sessions(&sessions_dir(root)).unwrap();
    assert_eq!(found(&store, "hunter2"), 1);

    // The transcript is re-redacted under stricter rules.
    write_session(
        root,
        "session-a",
        &[("user", "connect with token [REDACTED] to the server")],
    );
    store.index_sessions(&sessions_dir(root)).unwrap();

    assert_eq!(
        found(&store, "hunter2"),
        0,
        "content the new rules hide must not remain searchable"
    );
    assert_eq!(found(&store, "server"), 1, "the rest of the span survives");
}

#[test]
fn a_deleted_session_leaves_no_searchable_trace() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_session(root, "session-a", &[("user", "confidential deliberation")]);
    write_session(root, "session-b", &[("user", "kept material")]);
    let store = SpanStore::open(root).unwrap();
    store.index_sessions(&sessions_dir(root)).unwrap();
    let locator = span_locator(&store.search("confidential", 5).unwrap()[0]);

    std::fs::remove_dir_all(sessions_dir(root).join("session-a")).unwrap();
    store.index_sessions(&sessions_dir(root)).unwrap();

    assert_eq!(found(&store, "confidential"), 0);
    assert_eq!(found(&store, "kept"), 1);
    // And an outstanding locator says so rather than resolving to anything.
    assert_eq!(store.fetch_span(&locator).unwrap(), Err(SpanMiss::Gone));
}

// ------------------------------------------------------------- lifecycle

#[test]
fn a_session_that_grows_is_re_indexed_and_its_old_spans_do_not_linger() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_session(root, "session-a", &[("user", "opening statement")]);
    let store = SpanStore::open(root).unwrap();
    store.index_sessions(&sessions_dir(root)).unwrap();
    let before = store.span_count().unwrap();

    write_session(
        root,
        "session-a",
        &[("user", "opening statement"), ("assistant", "a reply")],
    );
    store.index_sessions(&sessions_dir(root)).unwrap();

    assert_eq!(store.span_count().unwrap(), before + 1);
    assert_eq!(found(&store, "opening"), 1, "no duplicate of the kept span");
}

#[test]
fn an_interrupted_build_is_recovered_by_the_next_run() {
    // The indexer dying mid-pass must not leave a session that *looks* complete.
    // A session's spans are written in one transaction and its state record is
    // written after, so a crash between them leaves spans with no state — and
    // the next run treats that as not-current and replaces them. The dangerous
    // ordering is the reverse, which would mark a session done before its spans
    // exist.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_session(root, "session-a", &[("user", "content to recover")]);
    let store = SpanStore::open(root).unwrap();
    store.index_sessions(&sessions_dir(root)).unwrap();

    // Simulate the crash window: spans present, state record lost.
    store.forget_index_state("session-a").unwrap();

    let report = store.index_sessions(&sessions_dir(root)).unwrap();
    assert_eq!(report.sessions_indexed, 1, "the next run must redo it");
    assert_eq!(
        found(&store, "recover"),
        1,
        "recovery must not duplicate the spans it re-writes"
    );
}

#[test]
fn two_sessions_sharing_a_prefix_are_indexed_independently() {
    // Fork. Two session directories are two sessions; a shared prefix produces
    // spans in both. Deliberate: they have separate lifetimes, and sharing rows
    // would mean deleting one session could remove the other's spans.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let shared = ("user", "the shared opening turn");
    write_session(root, "session-a", &[shared, ("assistant", "branch one")]);
    write_session(root, "session-b", &[shared, ("assistant", "branch two")]);
    let store = SpanStore::open(root).unwrap();
    store.index_sessions(&sessions_dir(root)).unwrap();

    assert_eq!(found(&store, "shared opening"), 2);

    std::fs::remove_dir_all(sessions_dir(root).join("session-a")).unwrap();
    store.index_sessions(&sessions_dir(root)).unwrap();
    assert_eq!(
        found(&store, "shared opening"),
        1,
        "deleting one fork must not remove the other's copy"
    );
}

#[test]
fn the_index_can_be_deleted_and_rebuilt_to_an_equivalent_state() {
    // The rollback story for every risk in this plan: the index is derived, and
    // proving it can be thrown away is what keeps it cheap to throw away.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for (index, id) in ["session-a", "session-b", "session-c"].iter().enumerate() {
        write_session(
            root,
            id,
            &[
                ("user", "a question about indexing"),
                ("assistant", "an answer"),
            ],
        );
        assert!(index < 3);
    }
    let (spans, sessions, sample) = {
        let store = SpanStore::open(root).unwrap();
        store.index_sessions(&sessions_dir(root)).unwrap();
        let sample: Vec<String> = store
            .search("indexing", 50)
            .unwrap()
            .iter()
            .map(span_locator)
            .collect();
        (
            store.span_count().unwrap(),
            store.session_count().unwrap(),
            sample,
        )
    };
    assert!(spans > 0 && !sample.is_empty());

    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(sessions_dir(root).join(format!("spans.sqlite{suffix}")));
    }

    let rebuilt = SpanStore::open(root).unwrap();
    rebuilt.index_sessions(&sessions_dir(root)).unwrap();
    assert_eq!(rebuilt.span_count().unwrap(), spans);
    assert_eq!(rebuilt.session_count().unwrap(), sessions);
    let after: Vec<String> = rebuilt
        .search("indexing", 50)
        .unwrap()
        .iter()
        .map(span_locator)
        .collect();
    assert_eq!(
        after, sample,
        "a rebuilt index must issue the same locators, or every stored one rots"
    );
}

#[test]
fn two_indexers_over_one_store_do_not_double_insert() {
    // Writer discipline: SQLite serialises writers and `busy_timeout` makes the
    // second wait rather than fail. The property that matters is not that both
    // succeed — it is that the store is never left with two copies of a span.
    //
    // Each thread opens its own connection, which is also the real shape: a
    // connection is not shared between threads, so two indexers are two
    // processes or two owned stores, never one borrowed across a boundary.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for id in ["session-a", "session-b", "session-c", "session-d"] {
        write_session(root, id, &[("user", "concurrently indexed content")]);
    }

    let one = root.to_path_buf();
    let two = root.to_path_buf();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            if let Ok(store) = SpanStore::open(&one) {
                let _ = store.index_sessions(&sessions_dir(&one));
            }
        });
        scope.spawn(move || {
            if let Ok(store) = SpanStore::open(&two) {
                let _ = store.index_sessions(&sessions_dir(&two));
            }
        });
    });

    let check = SpanStore::open(root).unwrap();
    check.index_sessions(&sessions_dir(root)).unwrap();
    assert_eq!(
        found(&check, "concurrently"),
        4,
        "one span per session regardless of how many indexers ran"
    );
    assert_eq!(check.session_count().unwrap(), 4);
}
