//! Which sessions worked a plan step, carried until the step commits.
//!
//! `PROGRESS.md` is written only when a step commits, but a step can span
//! several sessions: a quota pause, a blocked gate, or a cancelled turn ends the
//! invocation, and a later `resume` opens a new session for the same step. The
//! sessions seen so far wait in the project store's cache — outside the working
//! tree, so a pending step leaves nothing uncommitted behind — and move into the
//! step's `sessions:` line when it commits.
//!
//! The cache is a convenience, not the record. If it is lost, the step still
//! records the session that completed it; the earlier ones are then unknown,
//! and nothing downstream may read their absence as "there were none".

use serde::{Deserialize, Serialize};

use localpilot_core::SessionId;
use localpilot_store::Store;

/// The store key the pending step's sessions live under (an inspectable file
/// under `.localpilot/cache/`).
pub const STEP_SESSIONS_KEY: &str = "harness-step-sessions.json";

/// The sessions recorded against one not-yet-committed step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PendingStep {
    number: usize,
    /// Part of the match: a replanned or hand-edited plan can reuse a number
    /// for a different step, and its sessions must not be inherited.
    description: String,
    sessions: Vec<String>,
}

/// Record that `session` is working step `number`. Called once per session, at
/// the start of the step.
///
/// Best-effort: a failed write is logged and the step proceeds. The session
/// that completes the step is recorded regardless; only an earlier, abandoned
/// session could go unlinked.
pub fn note(store: &Store, number: usize, description: &str, session: SessionId) {
    let mut pending = read(store)
        .filter(|pending| pending.number == number && pending.description == description)
        .unwrap_or_else(|| PendingStep {
            number,
            description: description.to_string(),
            sessions: Vec::new(),
        });
    let session = session.to_string();
    if !pending.sessions.contains(&session) {
        pending.sessions.push(session);
    }
    let written = serde_json::to_vec(&pending)
        .map_err(|error| error.to_string())
        .and_then(|bytes| {
            store
                .put_cache(STEP_SESSIONS_KEY, &bytes)
                .map_err(|error| error.to_string())
        });
    if let Err(error) = written {
        tracing::warn!(
            target: "localpilot::harness",
            %error,
            "could not record the step's session; an earlier session of this step may go unlinked"
        );
    }
}

/// The sessions that worked step `number`, oldest first, ending with
/// `completing` — the list its `sessions:` line records. Read-only: the
/// pending entry is cleared by [`clear`] once the step's progress is
/// committed, so a failed write cannot lose the earlier sessions.
pub fn collect(
    store: &Store,
    number: usize,
    description: &str,
    completing: SessionId,
) -> Vec<String> {
    let mut sessions = read(store)
        .filter(|pending| pending.number == number && pending.description == description)
        .map(|pending| pending.sessions)
        .unwrap_or_default();
    let completing = completing.to_string();
    sessions.retain(|session| *session != completing);
    sessions.push(completing);
    sessions
}

/// Drop the pending entry once the step it describes has been committed.
pub fn clear(store: &Store) {
    if let Err(error) = store.delete_cache(STEP_SESSIONS_KEY) {
        tracing::warn!(
            target: "localpilot::harness",
            %error,
            "could not clear the committed step's pending sessions"
        );
    }
}

fn read(store: &Store) -> Option<PendingStep> {
    let bytes = store.get_cache(STEP_SESSIONS_KEY).ok().flatten()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resumed_step_collects_every_session_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path());
        let paused = SessionId::new();
        let completing = SessionId::new();

        note(&store, 2, "Implement parser errors", paused);
        note(&store, 2, "Implement parser errors", completing);
        let sessions = collect(&store, 2, "Implement parser errors", completing);

        assert_eq!(sessions, vec![paused.to_string(), completing.to_string()]);
        assert!(
            store.get_cache(STEP_SESSIONS_KEY).unwrap().is_some(),
            "collecting alone keeps the entry, in case the progress write fails"
        );
        clear(&store);
        assert!(store.get_cache(STEP_SESSIONS_KEY).unwrap().is_none());
    }

    #[test]
    fn a_different_step_does_not_inherit_the_pending_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path());
        let earlier = SessionId::new();
        let completing = SessionId::new();

        // Step 2 was replanned into a different step under the same number.
        note(&store, 2, "Implement parser errors", earlier);
        let sessions = collect(&store, 2, "Split the parser module", completing);

        assert_eq!(sessions, vec![completing.to_string()]);
    }

    #[test]
    fn a_lost_cache_still_records_the_completing_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path());
        let completing = SessionId::new();

        let sessions = collect(&store, 1, "Write failing test", completing);

        assert_eq!(sessions, vec![completing.to_string()]);
    }

    #[test]
    fn noting_the_same_session_twice_records_it_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path());
        let session = SessionId::new();

        note(&store, 1, "Write failing test", session);
        note(&store, 1, "Write failing test", session);

        assert_eq!(
            collect(&store, 1, "Write failing test", session),
            vec![session.to_string()]
        );
    }
}
