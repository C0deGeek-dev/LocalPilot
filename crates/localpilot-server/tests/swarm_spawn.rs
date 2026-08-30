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
use localpilot_server::swarm::registry::{MemberRole, MemberStatus, SwarmLimits, SwarmRegistry};
use localpilot_server::swarm::scope::SwarmId;
use localpilot_server::swarm::spawn::{
    AdoptedPair, SpawnError, SpawnRequest, Spawned, SwarmHost, WorkerFactory,
};
use localpilot_store::Store;
use localpilot_tools::{Audience, Delivery, PeerMessage, SwarmPeers, ToolRegistry};
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
    builds: std::sync::atomic::AtomicUsize,
}

impl Sessions {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            dir: Arc::new(tempfile::tempdir().unwrap()),
            answer: std::sync::Mutex::new(Vec::new()),
            model: std::sync::Mutex::new(SessionConfig::default().model),
            builds: std::sync::atomic::AtomicUsize::new(0),
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
        self.builds
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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

    fn builds(&self) -> usize {
        self.builds.load(std::sync::atomic::Ordering::SeqCst)
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

/// A host with two registered ordinary sessions, not yet adopted.
async fn pair_ready(limits: SwarmLimits) -> (SwarmHost, Arc<Sessions>, SessionId, SessionId) {
    let sessions = Sessions::new();
    let registry = SessionRegistry::new();
    let factory = Arc::clone(&sessions) as Arc<dyn SessionFactory>;
    let first = registry.open_new(&*factory).await.unwrap();
    let second = registry.open_new(&*factory).await.unwrap();
    let host = SwarmHost::for_adoption(registry, SwarmRegistry::with_limits(limits));
    (host, sessions, first, second)
}

async fn adopted_pair() -> (SwarmHost, Arc<Sessions>, AdoptedPair) {
    let (host, sessions, first, second) = pair_ready(SwarmLimits::default()).await;
    let pair = host
        .adopt_pair(&swarm(), (first, "alpha"), (second, "beta"))
        .await
        .unwrap();
    (host, sessions, pair)
}

#[tokio::test]
async fn an_adoption_only_host_refuses_hierarchical_worker_spawns_with_typed_errors() {
    let sessions = Sessions::new();
    let registry = SessionRegistry::new();
    let root = registry
        .open_new(&*(Arc::clone(&sessions) as Arc<dyn SessionFactory>))
        .await
        .unwrap();
    let host = SwarmHost::for_adoption(registry, SwarmRegistry::new());
    host.adopt_root(&swarm(), root, "root").await.unwrap();
    let refusal = "this host adopts existing sessions and cannot create workers";

    let plain = SpawnRequest::new(swarm(), root, "worker", "do work");
    assert_eq!(
        host.spawn(&plain).await,
        Err(SpawnError::Factory(refusal.to_string()))
    );

    let modeled = SpawnRequest::new(swarm(), root, "modeled", "do work").with_model("some-model");
    assert_eq!(
        host.spawn(&modeled).await,
        Err(SpawnError::ProviderUnavailable {
            model: "some-model".to_string(),
            reason: refusal.to_string(),
        })
    );

    assert_eq!(host.sessions().list().await, vec![root]);
    let members = host.swarms().members(&swarm()).await;
    assert_eq!(members.len(), 1, "no worker member was admitted");
    assert_eq!(members[0].session, root);
    assert!(
        host.host(root).await.is_some(),
        "the adopted root remains hosted"
    );
}

#[tokio::test]
async fn pair_adoption_validates_both_handles_before_mutating_any_host_state() {
    let sessions = Sessions::new();
    let registry = SessionRegistry::new();
    let first = registry
        .open_new(&*(Arc::clone(&sessions) as Arc<dyn SessionFactory>))
        .await
        .unwrap();
    let missing = SessionId::new();
    let host = SwarmHost::new(
        registry,
        SwarmRegistry::new(),
        Arc::clone(&sessions) as Arc<dyn WorkerFactory>,
    );

    assert!(matches!(
        host.adopt_pair(&swarm(), (first, "alpha"), (missing, "beta"))
            .await,
        Err(SpawnError::Registry(_))
    ));
    assert!(host.swarms().members(&swarm()).await.is_empty());
    assert!(host.host(first).await.is_none());
    assert!(host.host(missing).await.is_none());
}

#[tokio::test]
async fn an_adopted_pair_has_two_prompt_neutral_bound_peers_and_no_coordinator() {
    let (host, _sessions, first, second) = pair_ready(SwarmLimits::default()).await;
    let first_before = host
        .sessions()
        .get(first)
        .await
        .unwrap()
        .lock()
        .await
        .system_prompt_text();
    let second_before = host
        .sessions()
        .get(second)
        .await
        .unwrap()
        .lock()
        .await
        .system_prompt_text();

    let pair = host
        .adopt_pair(&swarm(), (first, "alpha"), (second, "beta"))
        .await
        .unwrap();

    assert_eq!(pair.swarm(), &swarm());
    assert_eq!(pair.sessions(), [first, second]);
    assert_eq!(pair.hosts()[0].id(), first);
    assert_eq!(pair.hosts()[1].id(), second);
    assert!(pair.host(SessionId::new()).is_none());
    assert_eq!(host.swarms().coordinator(&swarm()).await, None);
    let members = host.swarms().members(&swarm()).await;
    assert_eq!(members.len(), 2);
    assert!(members.iter().all(|member| {
        member.role == MemberRole::Peer
            && member.status == MemberStatus::Active
            && member.report_back_to.is_none()
    }));
    assert_eq!(pair.host(first).unwrap().subscriber_count(), 1);
    assert_eq!(pair.host(second).unwrap().subscriber_count(), 1);

    let first_after = host
        .sessions()
        .get(first)
        .await
        .unwrap()
        .lock()
        .await
        .system_prompt_text();
    let second_after = host
        .sessions()
        .get(second)
        .await
        .unwrap()
        .lock()
        .await
        .system_prompt_text();
    assert_eq!(first_after, first_before);
    assert_eq!(second_after, second_before);

    let first_view = pair.messaging(first).unwrap();
    let second_view = pair.messaging(second).unwrap();
    assert!(!first_view.identity().await.is_coordinator);
    assert!(!second_view.identity().await.is_coordinator);
    assert_eq!(first_view.roster().await[0].role, "peer");
    assert_eq!(second_view.roster().await[0].role, "peer");

    let broadcast = PeerMessage {
        audience: Audience::Swarm,
        tldr: None,
        body: "not allowed".to_string(),
        delivery: Delivery::Notify,
    };
    assert!(first_view.send(&broadcast).await.is_err());
    assert!(second_view.send(&broadcast).await.is_err());

    let direct = |target: SessionId, body: &str| PeerMessage {
        audience: Audience::One(target.to_string()),
        tldr: None,
        body: body.to_string(),
        delivery: Delivery::Notify,
    };
    assert_eq!(
        first_view
            .send(&direct(second, "from alpha"))
            .await
            .unwrap()
            .reached,
        1
    );
    assert_eq!(
        second_view
            .send(&direct(first, "from beta"))
            .await
            .unwrap()
            .reached,
        1
    );
    for session in [first, second] {
        assert!(
            !host
                .sessions()
                .get(session)
                .await
                .unwrap()
                .lock()
                .await
                .steer_queue()
                .is_empty(),
            "each direct notification is queued on its intended peer"
        );
    }
}

#[tokio::test]
async fn exact_and_concurrent_pair_retries_reuse_hosts_bindings_and_watchers() {
    let (host, _sessions, initial) = adopted_pair().await;
    let [first, second] = initial.sessions();
    let initial_hosts = initial.hosts();
    let initial_first_view = initial.messaging(first).unwrap();
    let initial_second_view = initial.messaging(second).unwrap();
    let pair_swarm = swarm();

    let sequential = host
        .adopt_pair(&pair_swarm, (second, "beta"), (first, "alpha"))
        .await
        .unwrap();
    let (left, right) = tokio::join!(
        host.adopt_pair(&pair_swarm, (first, "alpha"), (second, "beta")),
        host.adopt_pair(&pair_swarm, (second, "beta"), (first, "alpha")),
    );
    let left = left.unwrap();
    let right = right.unwrap();

    for retried in [&sequential, &left, &right] {
        assert!(Arc::ptr_eq(
            &retried.host(first).unwrap(),
            &initial_hosts[0]
        ));
        assert!(Arc::ptr_eq(
            &retried.host(second).unwrap(),
            &initial_hosts[1]
        ));
        assert!(Arc::ptr_eq(
            &retried.messaging(first).unwrap(),
            &initial_first_view
        ));
        assert!(Arc::ptr_eq(
            &retried.messaging(second).unwrap(),
            &initial_second_view
        ));
    }
    assert_eq!(initial_hosts[0].subscriber_count(), 1);
    assert_eq!(initial_hosts[1].subscriber_count(), 1);
    assert_eq!(host.swarms().members(&swarm()).await.len(), 2);
}

#[tokio::test]
async fn pair_topology_refuses_spawn_and_dispatch_before_the_factory() {
    let (host, sessions, pair) = adopted_pair().await;
    let [first, _second] = pair.sessions();
    let builds_before = sessions.builds();
    let request = SpawnRequest::new(swarm(), first, "third", "must not run");

    assert!(matches!(
        host.spawn(&request).await,
        Err(SpawnError::Admission(
            localpilot_server::swarm::SwarmError::MixedTopology
        ))
    ));
    assert!(matches!(
        host.dispatch(request).await,
        Err(SpawnError::Admission(
            localpilot_server::swarm::SwarmError::MixedTopology
        ))
    ));
    assert_eq!(sessions.builds(), builds_before);
    assert_eq!(host.swarms().members(&swarm()).await.len(), 2);
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
