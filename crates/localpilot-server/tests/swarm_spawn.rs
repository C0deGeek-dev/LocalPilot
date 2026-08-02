//! Spawning real workers, running them in parallel, and getting their answers
//! home — end to end, over actual `SessionRuntime`s driven by a scripted
//! provider.
//!
//! The unit tests in `swarm::spawn` cover the refusal paths, where no session is
//! ever built. These cover the paths that only exist once one is: a worker that
//! runs a turn, several that run at the same time, and a report that has to
//! reach a coordinator whose own turn is already in flight.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use localpilot_core::SessionId;
use localpilot_harness::{SessionConfig, SessionRuntime};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_server::registry::{RegistryError, SessionFactory, SessionRegistry};
use localpilot_server::swarm::registry::{MemberStatus, SwarmLimits, SwarmRegistry};
use localpilot_server::swarm::scope::SwarmId;
use localpilot_server::swarm::spawn::{
    SpawnError, SpawnRequest, Spawned, SwarmHost, WorkerFactory,
};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use tempfile::TempDir;

/// Builds sessions over one shared temp-dir store, each with its own scripted
/// provider answer, so a test can say what each worker will report.
struct Sessions {
    dir: Arc<TempDir>,
    /// What the next-built session's single turn answers with.
    answer: std::sync::Mutex<Vec<String>>,
    /// Model reported by the built session, so the override check is testable
    /// without a real provider.
    model: std::sync::Mutex<String>,
}

impl Sessions {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            dir: Arc::new(tempfile::tempdir().unwrap()),
            answer: std::sync::Mutex::new(Vec::new()),
            model: std::sync::Mutex::new(SessionConfig::default().model),
        })
    }

    /// Queue the answer the next spawned worker will give.
    fn will_answer(&self, text: &str) {
        self.answer.lock().unwrap().push(text.to_string());
    }

    fn set_model(&self, model: &str) {
        *self.model.lock().unwrap() = model.to_string();
    }

    fn build(&self) -> Result<SessionRuntime, String> {
        let root = self.dir.path();
        let workspace = Workspace::new(root).map_err(|err| err.to_string())?;
        let mut queued = self.answer.lock().unwrap();
        let text = if queued.is_empty() {
            "done".to_string()
        } else {
            queued.remove(0)
        };
        drop(queued);
        Ok(SessionRuntime::new(
            Arc::new(FakeProvider::new().text(&text)),
            ToolRegistry::with_builtins(),
            PermissionEngine::new(Profile::Bypass, Vec::new()),
            Box::new(ScriptedApprover::always()),
            Store::open(root),
            workspace,
            RecoveryEngine::new(RecoveryBudget::default()),
            SessionConfig {
                interactivity: Interactivity::NonInteractive,
                trusted: true,
                model: self.model.lock().unwrap().clone(),
                ..SessionConfig::default()
            },
            Vec::new(),
        ))
    }
}

impl WorkerFactory for Sessions {
    fn create(&self, _request: &SpawnRequest) -> Result<SessionRuntime, String> {
        self.build()
    }
}

impl SessionFactory for Sessions {
    fn create(&self) -> Result<SessionRuntime, RegistryError> {
        self.build().map_err(RegistryError::Factory)
    }
}

fn swarm() -> SwarmId {
    SwarmId::new("integration-swarm")
}

/// A swarm host plus a registered, adopted coordinator.
async fn coordinated(limits: SwarmLimits) -> (SwarmHost, Arc<Sessions>, SessionId) {
    let sessions = Sessions::new();
    let registry = SessionRegistry::new();
    let coordinator = registry
        .open_new(&*(Arc::clone(&sessions) as Arc<dyn SessionFactory>))
        .await
        .unwrap();
    let host = SwarmHost::new(
        registry,
        SwarmRegistry::with_limits(limits),
        Arc::clone(&sessions) as Arc<dyn WorkerFactory>,
    );
    host.adopt_root(&swarm(), coordinator, "lead")
        .await
        .unwrap();
    (host, sessions, coordinator)
}

#[tokio::test]
async fn a_spawned_worker_runs_a_turn_and_is_tracked_as_a_member() {
    let (host, sessions, coordinator) = coordinated(SwarmLimits::default()).await;
    sessions.will_answer("the parser lives in src/parse.rs");

    let request = SpawnRequest::new(swarm(), coordinator, "surveyor", "Find the parser.");
    let Spawned::Started { session } = host.spawn(&request).await.unwrap() else {
        panic!("a fresh spawn starts a worker");
    };

    let member = host.swarms().member(&swarm(), session).await.unwrap();
    assert_eq!(member.name, "surveyor");
    assert_eq!(member.report_back_to, Some(coordinator));
    assert_eq!(member.status, MemberStatus::Active);
    assert_eq!(
        host.swarms().children(&swarm(), coordinator).await,
        vec![session]
    );

    let report = host.run(&swarm(), session, &request.task).await.unwrap();
    assert_eq!(report.session, session);
    assert!(report.summary.contains("src/parse.rs"));
    assert!(!report.truncated);
    assert_eq!(report.delivered_to, Some(coordinator));
}

#[tokio::test]
async fn a_workers_report_reaches_the_coordinator_and_is_labelled_as_a_worker_report() {
    let (host, sessions, coordinator) = coordinated(SwarmLimits::default()).await;
    sessions.will_answer("two problems, both in the error path");

    let request = SpawnRequest::new(swarm(), coordinator, "reviewer", "Review it.");
    let (session, handle) = host.dispatch(request).await.unwrap();
    let report = handle.await.unwrap().unwrap();

    assert_eq!(report.delivered_to, Some(coordinator));
    // The membership record carries the answer too, so a coordinator that was
    // not around to receive the message can still read it.
    let member = host.swarms().member(&swarm(), session).await.unwrap();
    assert_eq!(member.status, MemberStatus::Finished);
    assert_eq!(
        member.completion.as_deref(),
        Some("two problems, both in the error path")
    );

    // And it really landed on the coordinator's soft-interrupt queue rather than
    // being reported as delivered and dropped — the queue is what the next turn
    // drains at its first safe boundary.
    let queue = host
        .sessions()
        .get(coordinator)
        .await
        .unwrap()
        .lock()
        .await
        .steer_queue();
    assert!(
        !queue.is_empty(),
        "the coordinator has a pending injection waiting for its next safe point"
    );
}

#[tokio::test]
async fn several_workers_run_at_the_same_time_within_the_budget() {
    let (host, sessions, coordinator) = coordinated(SwarmLimits {
        max_members: 16,
        max_active: 4,
    })
    .await;
    for index in 0..3 {
        sessions.will_answer(&format!("finding {index}"));
    }

    let mut running = Vec::new();
    for index in 0..3 {
        let request = SpawnRequest::new(
            swarm(),
            coordinator,
            format!("w{index}"),
            format!("Do part {index}."),
        );
        running.push(host.dispatch(request).await.unwrap());
    }

    let mut summaries = Vec::new();
    for (_, handle) in running {
        summaries.push(handle.await.unwrap().unwrap().summary);
    }
    summaries.sort();
    assert_eq!(summaries.len(), 3);
    assert!(summaries.iter().all(|s| s.starts_with("finding")));

    // The coordinator plus three workers, all in one swarm, all under one root.
    assert_eq!(host.swarms().members(&swarm()).await.len(), 4);
    assert_eq!(host.swarms().subtree(&swarm(), coordinator).await.len(), 4);
}

#[tokio::test]
async fn the_budget_refuses_the_spawn_that_would_exceed_it() {
    // One slot, and the coordinator itself is holding it.
    let (host, _sessions, coordinator) = coordinated(SwarmLimits {
        max_members: 16,
        max_active: 1,
    })
    .await;

    let request = SpawnRequest::new(swarm(), coordinator, "w", "work");
    assert!(matches!(
        host.spawn(&request).await,
        Err(SpawnError::Admission(_))
    ));
    assert_eq!(host.swarms().members(&swarm()).await.len(), 1);
}

#[tokio::test]
async fn a_spawn_that_asked_for_a_model_refuses_to_run_on_a_different_one() {
    let (host, sessions, coordinator) = coordinated(SwarmLimits::default()).await;
    sessions.set_model("the-model-that-was-configured");

    let request = SpawnRequest::new(swarm(), coordinator, "w", "work")
        .with_model("the-model-that-was-asked-for");
    let error = host.spawn(&request).await.unwrap_err();

    assert_eq!(
        error,
        SpawnError::ModelMismatch {
            requested: "the-model-that-was-asked-for".into(),
            actual: "the-model-that-was-configured".into(),
        }
    );
    assert_eq!(
        host.swarms().members(&swarm()).await.len(),
        1,
        "no worker was admitted"
    );

    // The same request succeeds once the models agree, which proves the refusal
    // was about the mismatch and not about the override being set at all.
    sessions.set_model("the-model-that-was-asked-for");
    assert!(matches!(
        host.spawn(&request).await.unwrap(),
        Spawned::Started { .. }
    ));
}

#[tokio::test]
async fn a_retried_spawn_produces_one_worker_not_two() {
    let (host, sessions, coordinator) = coordinated(SwarmLimits::default()).await;
    sessions.will_answer("the only answer");

    let request = SpawnRequest::new(swarm(), coordinator, "w", "work").with_key("spawn-attempt-1");
    let Spawned::Started { session } = host.spawn(&request).await.unwrap() else {
        panic!("first attempt starts");
    };

    assert_eq!(
        host.spawn(&request).await.unwrap(),
        Spawned::Already { session },
        "the retry is answered with the first attempt's worker"
    );
    assert_eq!(
        host.swarms().members(&swarm()).await.len(),
        2,
        "the coordinator and exactly one worker"
    );
}

#[tokio::test]
async fn a_report_with_nowhere_to_go_is_still_recorded() {
    let (host, sessions, coordinator) = coordinated(SwarmLimits::default()).await;
    sessions.will_answer("finished anyway");

    let request = SpawnRequest::new(swarm(), coordinator, "w", "work");
    let Spawned::Started { session } = host.spawn(&request).await.unwrap() else {
        panic!("started");
    };

    // The coordinator goes away mid-flight — the case the failure lifecycle
    // exists for. The worker must still finish and its answer must still be
    // readable, because a re-elected coordinator will need it.
    host.unhost(coordinator).await;

    let report = host.run(&swarm(), session, &request.task).await.unwrap();
    assert_eq!(report.delivered_to, None);
    assert_eq!(
        host.swarms()
            .member(&swarm(), session)
            .await
            .unwrap()
            .completion
            .as_deref(),
        Some("finished anyway")
    );
}

#[tokio::test]
async fn a_worker_is_an_ordinary_session_the_rest_of_the_server_can_see() {
    let (host, _sessions, coordinator) = coordinated(SwarmLimits::default()).await;
    let request = SpawnRequest::new(swarm(), coordinator, "w", "work");
    let Spawned::Started { session } = host.spawn(&request).await.unwrap() else {
        panic!("started");
    };

    // In the session registry, with a host, cancellable — nothing about it is a
    // special case.
    assert!(host.sessions().get(session).await.is_some());
    let worker_host = host.host(session).await.unwrap();
    assert_eq!(worker_host.id(), session);
    assert!(!worker_host.cancel(), "idle, so there is no turn to cancel");
    assert_eq!(host.swarms().swarm_of(session).await, Some(swarm()));
}
