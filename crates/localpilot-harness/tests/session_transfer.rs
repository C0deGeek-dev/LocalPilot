//! Exclusive session ownership and the idle hand-over of a session to another
//! process, exercised against the store with the destination's side played by
//! the test (the real destination is the workspace server).
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::Arc;

use localpilot_harness::{
    Admission, SessionConfig, SessionRuntime, StopReason, SwitchError, TransferRefusal,
};
use localpilot_llm::{FakeProvider, ReasoningEffort};
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::{Holder, HolderKind, SessionEventKind, Store, StoreError, TransferState};
use localpilot_tools::ToolRegistry;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

fn runtime(root: &Path, provider: FakeProvider) -> SessionRuntime {
    SessionRuntime::new(
        Arc::new(provider),
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Default, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(root),
        Workspace::new(root).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig {
            model: "m".to_string(),
            ..SessionConfig::default()
        },
        Vec::new(),
    )
}

async fn turn(runtime: &mut SessionRuntime, prompt: &str) -> StopReason {
    let (events, _rx) = broadcast::channel(64);
    runtime
        .run_turn(prompt, &events, &CancellationToken::new())
        .await
}

fn server() -> Holder {
    Holder::this_process(HolderKind::Server)
}

#[tokio::test]
async fn a_released_session_is_fenced_here_and_taken_over_by_the_destination() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = runtime(dir.path(), FakeProvider::new().text("hello"));
    assert_eq!(turn(&mut source, "hi").await, StopReason::Done);
    let session = source.session_id();
    let store = Store::open(dir.path());
    assert!(
        matches!(
            store.claim_session(session, server()),
            Err(StoreError::SessionBusy { .. })
        ),
        "an owned session is busy for every other opener"
    );

    let frozen = source.prepare_transfer("t1").unwrap();
    assert!(matches!(source.admission(), Admission::Preparing { .. }));
    source.release_transfer(&frozen).unwrap();
    assert!(matches!(source.admission(), Admission::Released { .. }));
    let events_after_release = store.read_events(session).unwrap().len();

    // The destination takes the lock and finds this transfer released, with
    // the frozen tail and digest.
    let mut destination = store.claim_session(session, server()).unwrap().unwrap();
    let released = destination.previous().unwrap().transfer.clone().unwrap();
    assert_eq!(released.id, "t1");
    assert_eq!(released.state, TransferState::Released);
    assert_eq!(released.tail, frozen.tail);
    assert_eq!(
        released.tail,
        store.read_events(session).unwrap().last().map(|e| e.id)
    );
    assert_eq!(released.manifest_digest, frozen.manifest.digest());
    destination.commit_transfer(released).unwrap();

    // The source can no longer run anything or write anything.
    assert_eq!(turn(&mut source, "again").await, StopReason::NotOwner);
    assert!(matches!(
        source.set_active_model("other"),
        Err(SwitchError::NotOwner)
    ));
    let shell = source
        .run_user_shell_command_detailed("echo x", &CancellationToken::new(), false)
        .await;
    assert!(shell.result.is_error());
    source.close();
    assert_eq!(
        store.read_events(session).unwrap().len(),
        events_after_release,
        "a released runtime writes nothing, not even SessionClosed"
    );
}

#[tokio::test]
async fn preparing_refuses_execution_and_mutation_until_aborted() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = runtime(dir.path(), FakeProvider::new().text("after abort"));
    source.prepare_transfer("t1").unwrap();

    assert_eq!(turn(&mut source, "now").await, StopReason::NotOwner);
    assert!(matches!(
        source.set_active_model("other"),
        Err(SwitchError::NotOwner)
    ));
    source.set_trusted(!source.trusted());
    source.set_reasoning_effort(Some(ReasoningEffort::High));
    let manifest = source.transfer_manifest();
    assert_eq!(
        manifest.trusted,
        SessionConfig::default().trusted,
        "trust unchanged"
    );
    assert_eq!(manifest.reasoning_effort, None, "effort unchanged");
    assert!(matches!(
        source.prepare_transfer("t2"),
        Err(TransferRefusal::NotOwned)
    ));

    assert!(!source.abort_transfer("other"));
    assert!(source.abort_transfer("t1"));
    assert_eq!(source.admission(), Admission::Owned);
    assert_eq!(turn(&mut source, "now").await, StopReason::Done);
}

#[tokio::test]
async fn a_refused_handle_change_leaves_the_frozen_session_releasable() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = runtime(dir.path(), FakeProvider::new());
    let effort = source.reasoning_effort_handle();
    let frozen = source.prepare_transfer("t1").unwrap();
    assert!(
        !effort.set(Some(ReasoningEffort::Low)),
        "refused while preparing"
    );
    source.release_transfer(&frozen).unwrap();
    assert!(matches!(source.admission(), Admission::Released { .. }));
    assert!(
        !effort.set(Some(ReasoningEffort::Low)),
        "refused once released"
    );
}

#[tokio::test]
async fn queued_steering_or_incognito_refuses_the_hand_over() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = runtime(dir.path(), FakeProvider::new());
    source.steer_queue().push("later");
    assert!(matches!(
        source.prepare_transfer("t1"),
        Err(TransferRefusal::SteerQueued)
    ));
    assert_eq!(source.admission(), Admission::Owned);

    let mut private = runtime(dir.path(), FakeProvider::new());
    assert!(private.enter_incognito());
    assert!(matches!(
        private.prepare_transfer("t1"),
        Err(TransferRefusal::Incognito | TransferRefusal::Unclaimed)
    ));
}

#[tokio::test]
async fn a_failed_release_publish_keeps_the_source_owner_and_holder() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = runtime(dir.path(), FakeProvider::new().text("still mine"));
    let session = source.session_id();
    let frozen = source.prepare_transfer("t1").unwrap();
    let record = dir
        .path()
        .join(".localpilot/sessions")
        .join(format!("{session}.owner.json"));
    std::fs::remove_file(&record).unwrap();
    std::fs::create_dir(&record).unwrap();
    std::fs::write(record.join("block"), b"x").unwrap();

    assert!(matches!(
        source.release_transfer(&frozen),
        Err(TransferRefusal::Store(_))
    ));
    assert_eq!(source.admission(), Admission::Owned);
    assert!(matches!(
        Store::open(dir.path()).claim_session(session, server()),
        Err(StoreError::SessionBusy { .. })
    ));
    assert_eq!(turn(&mut source, "go").await, StopReason::Done);
}

#[tokio::test]
async fn a_session_another_runtime_owns_cannot_be_resumed_and_nothing_changes() {
    let dir = tempfile::tempdir().unwrap();
    let owner = runtime(dir.path(), FakeProvider::new());
    let mut other = runtime(dir.path(), FakeProvider::new());
    let before = other.session_id();
    assert!(matches!(
        other.load_session(owner.session_id()),
        Err(StoreError::SessionBusy { .. })
    ));
    assert_eq!(other.session_id(), before);
    assert_eq!(other.admission(), Admission::Owned);
}

#[tokio::test]
async fn the_source_cold_resumes_when_the_destination_never_committed() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = runtime(dir.path(), FakeProvider::new().text("one").text("two"));
    assert_eq!(turn(&mut source, "first").await, StopReason::Done);
    let session = source.session_id();
    let frozen = source.prepare_transfer("t1").unwrap();
    source.release_transfer(&frozen).unwrap();
    // The destination died before committing: the lock is free and the record
    // says released. The source takes the session back by resuming it.
    let before = Store::open(dir.path())
        .owner_record(session)
        .unwrap()
        .unwrap()
        .generation;
    source.load_session(session).unwrap();
    assert_eq!(source.admission(), Admission::Owned);
    let record = Store::open(dir.path())
        .owner_record(session)
        .unwrap()
        .unwrap();
    assert_eq!(record.generation, before + 1);
    assert!(record.transfer.is_none());
    assert_eq!(turn(&mut source, "second").await, StopReason::Done);
    let messages = source
        .store()
        .read_events(session)
        .unwrap()
        .into_iter()
        .filter(|e| matches!(e.kind, SessionEventKind::Message { .. }))
        .count();
    assert!(messages >= 4, "both exchanges are in one log");
}

// --- review regressions: the freeze is a barrier at every mutation ----------

#[tokio::test]
async fn preparing_refuses_session_switches_and_history_changes() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = runtime(dir.path(), FakeProvider::new().text("a"));
    assert_eq!(turn(&mut source, "hi").await, StopReason::Done);
    let session = source.session_id();
    source.prepare_transfer("t1").unwrap();
    let history = source.context_usage().0;

    source.start_new_session();
    assert_eq!(
        source.session_id(),
        session,
        "no new session while preparing"
    );
    assert!(!source.enter_incognito());
    source.clear_conversation();
    let _ = source.compact_conversation_force().await;
    assert_eq!(source.context_usage().0, history, "history unchanged");
    assert!(matches!(source.admission(), Admission::Preparing { .. }));
}

#[tokio::test]
async fn handles_cloned_before_preparing_are_refused_at_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = runtime(dir.path(), FakeProvider::new());
    let effort = source.reasoning_effort_handle();
    let engine = source.permission_engine_handle();
    let steer = source.steer_queue();
    source.prepare_transfer("t1").unwrap();

    effort.set(Some(ReasoningEffort::Low));
    engine.set(engine.snapshot().with_profile(Profile::Bypass, Vec::new()));
    steer.push("sneaked in");
    assert_eq!(effort.snapshot(), None);
    assert_eq!(engine.snapshot().profile(), Profile::Default);
    assert!(steer.is_empty());
}

#[tokio::test]
async fn a_runtime_that_cannot_claim_its_session_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    // A file where the sessions directory must be: no lock can be opened.
    std::fs::create_dir_all(dir.path().join(".localpilot")).unwrap();
    std::fs::write(dir.path().join(".localpilot/sessions"), b"x").unwrap();
    let mut source = runtime(dir.path(), FakeProvider::new().text("must not run"));
    assert_eq!(source.admission(), Admission::Lost);
    assert_eq!(turn(&mut source, "hi").await, StopReason::NotOwner);
}

// --- review regressions: close never undoes a freeze or leaves a live runtime

#[tokio::test]
async fn close_while_preparing_keeps_the_session_frozen_and_claimed() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = runtime(dir.path(), FakeProvider::new());
    let session = source.session_id();
    let frozen = source.prepare_transfer("t1").unwrap();
    source.close();
    assert!(matches!(source.admission(), Admission::Preparing { .. }));
    assert!(matches!(
        Store::open(dir.path()).claim_session(session, server()),
        Err(StoreError::SessionBusy { .. })
    ));
    source.release_transfer(&frozen).unwrap();
    assert!(Store::open(dir.path())
        .claim_session(session, server())
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn a_closed_runtime_runs_nothing_until_it_claims_a_session_again() {
    let dir = tempfile::tempdir().unwrap();
    let mut first = runtime(dir.path(), FakeProvider::new().text("one").text("three"));
    assert_eq!(turn(&mut first, "hi").await, StopReason::Done);
    let session = first.session_id();
    first.close();
    assert_eq!(first.admission(), Admission::Closed);

    // Another runtime takes the session; the closed one cannot touch it.
    let mut second = runtime(dir.path(), FakeProvider::new().text("two"));
    second.load_session(session).unwrap();
    assert_eq!(turn(&mut first, "sneak").await, StopReason::NotOwner);
    let shell = first
        .run_user_shell_command_detailed("echo x", &CancellationToken::new(), false)
        .await;
    assert!(shell.result.is_error());
    assert_eq!(turn(&mut second, "go").await, StopReason::Done);

    // Starting a new session claims it first and runs again.
    assert!(first.start_new_session());
    assert_ne!(first.session_id(), session);
    assert_eq!(turn(&mut first, "fresh").await, StopReason::Done);
}
