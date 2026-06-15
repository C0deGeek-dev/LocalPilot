//! End-to-end regression fixture for the learning loop.
//!
//! This pins the one behaviour the product exists for: a closed-out session
//! becomes a reviewable candidate, an accepted candidate is promoted to durable
//! Markdown memory with an audit trail, and that memory is retrievable on a
//! later turn. It is the durable proof that the loop closes — before this work,
//! the canonical workspace had 0 promoted memories, 0 skills, and 0 audit
//! events after seven closed-out sessions.
//!
//! The golden session below is original to this repository (no captured
//! workspace content) and is reused by the memory-quality evaluation.

use localpilot_core::{Message, Role, SessionId};
use localpilot_localmind::{
    audit, closeout_session, context_for, memory_list, promote, review_decide, review_list, search,
    ReviewVerdict,
};
use localpilot_store::Store;

/// The golden session: a real exporter bug, its fix, and an explicit lesson.
const GOLDEN_SESSION: &[(Role, &str)] = &[
    (Role::User, "the exporter test keeps failing on empty parquet files"),
    (
        Role::Assistant,
        "error: assertion failed: row_groups == 0 in exporter/src/writer.rs",
    ),
    (
        Role::Assistant,
        "Fixed: flush the batch before clearing the buffer at the capacity boundary; the suite is passing now.",
    ),
    (
        Role::User,
        "Lesson: exporter changes need the integration suite, the unit tests miss schema drift.",
    ),
];

/// The promoted lesson must be retrievable by these terms on a later turn.
const RETRIEVAL_QUERY: &str = "exporter integration suite";

#[test]
fn learning_loop_closes_and_retrieves_a_promoted_memory() {
    let dir = tempfile::tempdir().expect("temp dir");
    let root = dir.path();
    let store = Store::open(root);

    // 1. Capture the session.
    let session = SessionId::new();
    for (role, text) in GOLDEN_SESSION {
        store
            .append_message(session, &Message::text(*role, *text))
            .expect("append message");
    }

    // Precondition (the documented "never closed" starting state): no promoted
    // memory and no audit events exist yet.
    assert!(memory_list(root).expect("memory list").is_empty());
    assert!(audit(root).expect("audit").is_empty());
    assert!(!root.join(".localmind/memory").exists());

    // 2. Close out → candidates enqueued for review.
    let summary = closeout_session(root, &store, session).expect("closeout");
    assert!(
        summary.enqueued_count >= 1,
        "closeout should enqueue at least the explicit lesson, got {summary:?}"
    );

    // 3. Find the explicit lesson among the review items.
    let items = review_list(root).expect("review list");
    let lesson = items
        .iter()
        .find(|item| item.summary.to_lowercase().contains("integration suite"))
        .expect("the explicit lesson should be a review candidate");

    // 4. Accept and 5. promote it to durable memory.
    review_decide(root, &lesson.id, ReviewVerdict::Accept, "tester", None).expect("accept");
    let memory_id = promote(root, &lesson.id).expect("promote");
    assert!(!memory_id.is_empty(), "promote returns a memory id");

    // 6a. A Markdown memory file now exists on disk.
    let memory_files: Vec<_> = walk_markdown(&root.join(".localmind/memory"));
    assert!(
        !memory_files.is_empty(),
        "promotion must write a .localmind/memory/*.md file"
    );

    // 6b. The memory is listed and carries the lesson text.
    let memories = memory_list(root).expect("memory list");
    assert!(
        memories
            .iter()
            .any(|m| m.body.to_lowercase().contains("integration suite")),
        "promoted memory should carry the lesson: {memories:?}"
    );

    // 6c. An audit event records the promotion.
    let audit_events = audit(root).expect("audit");
    assert!(
        !audit_events.is_empty(),
        "promotion must write an audit event"
    );

    // 7. The memory is retrievable on a later turn (keyword search + context).
    let hits = search(root, RETRIEVAL_QUERY).expect("search");
    assert!(
        hits.iter().any(|hit| hit.memory_id == memory_id),
        "promoted memory should be retrievable by search: {hits:?}"
    );
    let context = context_for(root, RETRIEVAL_QUERY).expect("context");
    assert!(
        context
            .as_deref()
            .is_some_and(|c| c.to_lowercase().contains("integration suite")),
        "context hook should surface the promoted memory: {context:?}"
    );
}

/// Collect every `.md` file under `dir` (recursively). Returns empty if absent.
fn walk_markdown(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk_markdown(&path));
            } else if path.extension().is_some_and(|ext| ext == "md") {
                out.push(path);
            }
        }
    }
    out
}
