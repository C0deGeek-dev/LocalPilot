//! Seed retrieved LocalMind context into a session before a turn.

use std::path::Path;

use localpilot_harness::SessionRuntime;

/// Inject relevant accepted project memory for `query` as a system message. A
/// no-op when nothing matches or on any retrieval error (best-effort context,
/// never fatal to the turn).
pub fn seed(cwd: &Path, runtime: &mut SessionRuntime, query: &str) {
    if let Ok(Some(context)) = localpilot_localmind::context_for(cwd, query) {
        runtime.seed_system(context);
    }
}

/// Close out a finished session into LocalMind: extract candidate lessons and
/// enqueue them for review. Best-effort and non-fatal; a no-op when the session
/// produced no transcript. The interactive REPL (the `tui` feature) is the
/// consumer.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
pub fn close_out(cwd: &Path, session: localpilot_core::SessionId) {
    let store = localpilot_store::Store::open(cwd);
    // Skip an empty session so opening and closing the REPL leaves no artifacts.
    if store
        .read_transcript(session)
        .map(|m| m.is_empty())
        .unwrap_or(true)
    {
        return;
    }
    match localpilot_localmind::closeout_session(cwd, &store, session) {
        Ok(summary) => eprintln!(
            "learning: closed out session — {} candidate(s), {} enqueued for review",
            summary.candidate_count, summary.enqueued_count
        ),
        Err(error) => eprintln!("learning: closeout skipped ({error})"),
    }
}
