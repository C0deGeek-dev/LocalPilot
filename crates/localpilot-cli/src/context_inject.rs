//! Session close-out into LocalMind.
//!
//! The pre-turn context hook now lives in `localpilot-localmind`
//! (`register_context_hook`); this module keeps the host-side session close-out
//! that runs on exit.

use std::path::Path;

/// Close out a finished session into LocalMind: extract candidate lessons and
/// enqueue them for review, then keep the code graph current. Best-effort and
/// non-fatal; a no-op when the session produced no transcript. Called on every
/// deliberate session-end path — the interactive REPL, each headless harness
/// step, and the RPC/ACP serve loop — so autonomous runs learn too, not just the
/// REPL. (One-shot `print` deliberately does not close out, so a bare prompt
/// never creates project files.)
pub fn close_out(cwd: &Path, session: localpilot_core::SessionId) {
    let store = localpilot_store::Store::open(cwd);
    // Skip an empty session so opening and closing a session leaves no artifacts.
    if store
        .read_transcript(session)
        .map(|m| m.is_empty())
        .unwrap_or(true)
    {
        return;
    }
    match localpilot_localmind::closeout_session(cwd, &store, session) {
        Ok(summary) => {
            // Record the just-closed session so on-demand `knowledge_search` in
            // any later turn of this run excludes the in-progress conversation
            // instead of echoing it back as project knowledge. Best-effort.
            let _ = localpilot_localmind::record_active_session(cwd, &summary.session_id);
            eprintln!(
                "learning: closed out session — {} candidate(s), {} enqueued, {} auto-accepted",
                summary.candidate_count, summary.enqueued_count, summary.accepted_count
            );
        }
        Err(error) => eprintln!("learning: closeout skipped ({error})"),
    }

    // Keep the code graph current while the workspace is quiet. Bounded so a
    // large edit burst cannot stall shutdown; leftovers wait for the next
    // session close, and an up-to-date graph is a cheap no-op.
    let graph_current = match localpilot_localmind::codegraph_reindex(cwd, CODEGRAPH_BATCH_LIMIT) {
        Ok(summary) => {
            if summary.reindexed + summary.pruned > 0 {
                eprintln!(
                    "learning: code graph updated — {} file(s) reindexed, {} pruned{}",
                    summary.reindexed,
                    summary.pruned,
                    if summary.remaining > 0 {
                        ", more queued for next session"
                    } else {
                        ""
                    }
                );
            }
            // Only distil once the graph is fully current, so the primer reflects
            // the whole repo rather than a partial batch.
            summary.remaining == 0
        }
        Err(error) => {
            eprintln!("learning: code graph reindex skipped ({error})");
            false
        }
    };

    // With a current graph, refresh the cold-start primer: distillation enqueues
    // a review candidate (gated by the project's learning flag); it is injected
    // only once a reviewer accepts it. Re-uses this existing close-out trigger.
    if graph_current {
        match localpilot_localmind::distill_primer_into_review(cwd) {
            Ok(Some(_)) => eprintln!("learning: repo primer enqueued for review"),
            Ok(None) => {}
            Err(error) => eprintln!("learning: primer distillation skipped ({error})"),
        }
    }
}

/// How many files one session-close reindex pass may touch.
const CODEGRAPH_BATCH_LIMIT: usize = 64;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use localpilot_core::{Message, Role, SessionId};
    use localpilot_store::Store;

    #[test]
    fn close_out_of_a_real_session_enqueues_review_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path());
        let session = SessionId::new();
        store
            .append_message(
                session,
                &Message::text(Role::User, "Lesson: redact secrets before persisting."),
            )
            .unwrap();

        // The shared helper that every non-REPL session-end path calls (headless
        // harness steps, the RPC serve loop) must learn from a real session, not
        // just the interactive REPL.
        close_out(dir.path(), session);

        let items = localpilot_localmind::review_list(dir.path()).unwrap();
        assert!(
            !items.is_empty(),
            "closeout of a real session must enqueue at least one review candidate"
        );
    }

    #[test]
    fn close_out_of_an_empty_session_creates_no_localmind_artifacts() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionId::new();

        // Opening and closing a bare session must leave no learning state, so a
        // plain prompt never creates project files.
        close_out(dir.path(), session);

        assert!(!dir.path().join(".localmind").exists());
        assert!(!dir.path().join(".localmind.toml").exists());
    }
}
