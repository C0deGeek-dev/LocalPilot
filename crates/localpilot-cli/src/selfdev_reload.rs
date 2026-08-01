//! The far side of a self-dev reload: when a session resumes, continue it
//! automatically if a reload left a continuation intent for it.
//!
//! The intent lifecycle is deliberately durable, idempotent, and non-consuming
//! (see `localpilot-selfdev`'s `reload` module). This module is the CLI seam that
//! puts it to work: at resume it garbage-collects delivered intents, checks for a
//! pending one, and — if found — runs the hidden continuation prompt as the
//! opening turn, recording delivery **only once the turn completes**. A restart
//! that dies mid-continuation leaves the intent pending, so the next resume tries
//! again; a restart that completes marks it delivered, so it is never replayed.
//!
//! **Not yet wired for now.** These functions are the *consumer* half of the
//! reload. Its *producer* — the in-session command that builds, vets, and swaps
//! onto a new binary, writing the intent this module reads — is the opt-in
//! autonomous self-dev loop, off by default (ADR-0128). The interactive/rpc resume
//! call that invokes `continue_if_pending` lands with that surface so the whole
//! loop turns on together, rather than wiring a consumer that nothing can yet
//! produce for. Fully unit-tested here against a real session runtime.
#![allow(dead_code)]

use localpilot_harness::{RuntimeEvent, SessionRuntime, StopReason};
use localpilot_selfdev::{ReloadIntent, ReloadStore};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// The self-dev reload store rooted at the standard per-user location, when the
/// platform reports one. `None` disables the continuation seam (there is nowhere
/// self-dev state could live), which is correct for a normal install.
#[must_use]
pub fn store() -> Option<ReloadStore> {
    localpilot_selfdev::default_root().map(ReloadStore::new)
}

/// Garbage-collect delivered intents and return the pending continuation for
/// `session_id`, if a reload left one. Non-consuming: the intent stays on disk
/// until [`record_delivered`] is called.
#[must_use]
pub fn take_pending(reload: &ReloadStore, session_id: &str) -> Option<ReloadIntent> {
    // Reclaim intents whose continuation already ran, on every resume — the
    // reference's "GC at start" discipline.
    reload.gc();
    reload.pending(session_id)
}

/// Record that a continuation was delivered and accepted. Idempotent.
pub fn record_delivered(reload: &ReloadStore, session_id: &str) {
    // A failure to record is not fatal: the worst case is the continuation runs
    // once more on the next resume, which the prompt is written to tolerate.
    let _ = reload.mark_delivered(session_id);
}

/// Continue a freshly resumed session if a reload is waiting for it.
///
/// Runs the intent's hidden continuation prompt as the opening turn, then records
/// delivery **only** if the turn finished cleanly (`Done`). A turn that was
/// cancelled, quiesced, or errored leaves the intent pending so a later resume
/// retries it — delivery is recorded on acceptance, never on the mere attempt.
///
/// Returns `true` if a continuation ran to completion.
pub async fn continue_if_pending(
    runtime: &mut SessionRuntime,
    reload: &ReloadStore,
    session_id: &str,
    events: &broadcast::Sender<RuntimeEvent>,
    cancel: &CancellationToken,
) -> bool {
    let Some(intent) = take_pending(reload, session_id) else {
        return false;
    };
    let reason = runtime
        .run_turn(&intent.continuation_prompt(), events, cancel)
        .await;
    if reason == StopReason::Done {
        record_delivered(reload, session_id);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_selfdev::ReloadIntent;

    fn intent(session: &str) -> ReloadIntent {
        ReloadIntent::new("req-1", session, "aaaa", "bbbb", "the task", None, 1)
    }

    fn intent_for(session: &str, task: &str) -> ReloadIntent {
        ReloadIntent::new("req-1", session, "2.6.0-a", "2.6.0-b", task, None, 1)
    }

    #[test]
    fn take_pending_gcs_delivered_and_returns_only_a_live_continuation() {
        let temp = tempfile::tempdir().unwrap();
        let reload = ReloadStore::new(temp.path());
        reload.write(&intent("live")).unwrap();
        reload.write(&intent("spent")).unwrap();
        reload.mark_delivered("spent").unwrap();

        // A spent intent is gc'd and never offered; a live one is returned without
        // being consumed.
        assert!(take_pending(&reload, "spent").is_none());
        let live = take_pending(&reload, "live").expect("a live continuation");
        assert_eq!(live.session_id, "live");
        assert!(
            take_pending(&reload, "live").is_some(),
            "taking a pending intent must not consume it"
        );
    }

    #[test]
    fn recording_delivery_stops_it_being_offered_again() {
        let temp = tempfile::tempdir().unwrap();
        let reload = ReloadStore::new(temp.path());
        reload.write(&intent("s")).unwrap();

        record_delivered(&reload, "s");
        assert!(take_pending(&reload, "s").is_none());
    }

    // --- end-to-end continuation against a real session runtime ---

    use std::sync::Arc;

    use localpilot_core::{ContentBlock, Role};
    use localpilot_llm::FakeProvider;
    use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
    use localpilot_sandbox::{PermissionEngine, Profile, ScriptedApprover, Workspace};
    use localpilot_store::Store;
    use localpilot_tools::ToolRegistry;

    fn runtime(dir: &std::path::Path, provider: FakeProvider) -> SessionRuntime {
        SessionRuntime::new(
            Arc::new(provider),
            ToolRegistry::with_builtins(),
            PermissionEngine::new(Profile::Bypass, Vec::new()),
            Box::new(ScriptedApprover::always()),
            Store::open(dir),
            Workspace::new(dir).unwrap(),
            RecoveryEngine::new(RecoveryBudget::default()),
            localpilot_harness::SessionConfig {
                trusted: true,
                ..localpilot_harness::SessionConfig::default()
            },
            Vec::new(),
        )
    }

    #[tokio::test]
    async fn a_pending_continuation_runs_as_the_opening_turn_and_is_then_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let reload = ReloadStore::new(dir.path().join("selfdev"));
        let store = Store::open(dir.path());

        let mut rt = runtime(dir.path(), FakeProvider::new().text("continuing now"));
        let session = rt.session_id().to_string();
        reload
            .write(&intent_for(&session, "finish the widget"))
            .unwrap();

        let (events, _rx) = broadcast::channel(64);
        let cancel = CancellationToken::new();
        let ran = continue_if_pending(&mut rt, &reload, &session, &events, &cancel).await;

        assert!(ran, "a pending continuation must run");
        assert!(
            take_pending(&reload, &session).is_none(),
            "a completed continuation must be marked delivered, not offered again"
        );

        // The opening turn was the continuation prompt, carrying the task.
        let transcript = store.read_transcript(rt.session_id()).unwrap();
        let first_user = transcript
            .iter()
            .find(|m| m.role == Role::User)
            .and_then(|m| {
                m.content.iter().find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
            })
            .expect("an opening user turn");
        assert!(
            first_user.contains("finish the widget") && first_user.contains("hot reload"),
            "the opening turn must be the continuation prompt: {first_user}"
        );
    }

    #[tokio::test]
    async fn with_no_pending_intent_a_normal_resume_runs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let reload = ReloadStore::new(dir.path().join("selfdev"));
        let mut rt = runtime(dir.path(), FakeProvider::new().text("unused"));
        let session = rt.session_id().to_string();

        let (events, _rx) = broadcast::channel(64);
        let cancel = CancellationToken::new();
        let ran = continue_if_pending(&mut rt, &reload, &session, &events, &cancel).await;

        assert!(!ran, "a session with no reload intent continues nothing");
    }
}
