//! A worker dies mid-plan, and the plan recovers.
//!
//! Everything here is the case that only matters when something goes wrong, so
//! each test breaks something on purpose: a worker stops answering, a
//! coordinator departs, a server restarts. What is asserted is that the *plan*
//! survives — the work comes back, the tree stays walkable, and somebody is told.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use localpilot_core::SessionId;
use localpilot_harness::{SessionConfig, SessionRuntime};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_server::registry::{RegistryError, SessionFactory, SessionRegistry};
use localpilot_server::swarm::lifecycle::{
    reap_terminal, salvage, sweep, SnapshotStore, DEFAULT_RECLAIM_LIMIT,
};
use localpilot_server::swarm::registry::{MemberStatus, SwarmLimits, SwarmRegistry};
use localpilot_server::swarm::scope::SwarmId;
use localpilot_server::swarm::spawn::{SpawnRequest, Spawned, SwarmHost, WorkerFactory};
use localpilot_store::Store;
use localpilot_taskgraph::ops::{seed, Salvage};
use localpilot_taskgraph::schedule::{dispatch, ready_nodes};
use localpilot_taskgraph::{ActorId, NodeSpec, PlanMode, TaskPlan, TaskStatus};
use tempfile::TempDir;

struct Sessions {
    dir: Arc<TempDir>,
}

impl Sessions {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            dir: Arc::new(tempfile::tempdir().unwrap()),
        })
    }

    fn build(&self) -> Result<SessionRuntime, String> {
        let root = self.dir.path();
        let workspace = Workspace::new(root).map_err(|err| err.to_string())?;
        Ok(SessionRuntime::new(
            Arc::new(FakeProvider::new().text("done")),
            localpilot_tools::ToolRegistry::with_builtins(),
            PermissionEngine::new(Profile::Bypass, Vec::new()),
            Box::new(ScriptedApprover::always()),
            Store::open(root),
            workspace,
            RecoveryEngine::new(RecoveryBudget::default()),
            SessionConfig {
                interactivity: Interactivity::NonInteractive,
                trusted: true,
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
    SwarmId::new("lifecycle-swarm")
}

async fn host_with(sessions: &Arc<Sessions>) -> (SwarmHost, SessionId) {
    let registry = SessionRegistry::new();
    let lead = registry
        .open_new(&*(Arc::clone(sessions) as Arc<dyn SessionFactory>))
        .await
        .unwrap();
    let host = SwarmHost::new(
        registry,
        SwarmRegistry::with_limits(SwarmLimits {
            max_members: 16,
            max_active: 16,
        }),
        Arc::clone(sessions) as Arc<dyn WorkerFactory>,
    );
    host.adopt_root(&swarm(), lead, "lead").await.unwrap();
    (host, lead)
}

async fn spawn(host: &SwarmHost, parent: SessionId, name: &str) -> SessionId {
    match host
        .spawn(&SpawnRequest::new(swarm(), parent, name, "work"))
        .await
        .unwrap()
    {
        Spawned::Started { session } => session,
        other => panic!("expected a fresh spawn, got {other:?}"),
    }
}

/// A two-task plan owned by `lead`, with both tasks dispatched to `worker`.
async fn plan_assigned_to(host: &SwarmHost, lead: SessionId, worker: SessionId) {
    let owner = ActorId::new(lead.to_string());
    let mut plan = TaskPlan::new("ship it", PlanMode::Light, owner.clone());
    seed(
        &mut plan,
        &owner,
        "seed-1",
        &[
            NodeSpec::task("first", "do the first half"),
            NodeSpec::task("second", "do the second half"),
        ],
    )
    .unwrap();
    let assignee = ActorId::new(worker.to_string());
    for node in ready_nodes(&plan) {
        dispatch(&mut plan, node, &assignee).unwrap();
    }
    host.swarms().set_plan(&swarm(), plan).await.unwrap();
}

async fn pending(host: &SwarmHost, session: SessionId) -> bool {
    !host
        .sessions()
        .get(session)
        .await
        .unwrap()
        .lock()
        .await
        .steer_queue()
        .is_empty()
}

#[tokio::test]
async fn a_dead_workers_tasks_come_back_and_the_coordinator_is_told() {
    let sessions = Sessions::new();
    let (host, lead) = host_with(&sessions).await;
    let worker = spawn(&host, lead, "alpha").await;
    plan_assigned_to(&host, lead, worker).await;

    let salvaged = salvage(&host, &swarm(), worker, DEFAULT_RECLAIM_LIMIT).await;

    assert_eq!(salvaged.departed, worker);
    assert_eq!(salvaged.tasks.len(), 2);
    assert!(salvaged
        .tasks
        .iter()
        .all(|(_, outcome)| matches!(outcome, Salvage::Requeued { reclaims: 1 })));
    assert_eq!(salvaged.reported_to, Some(lead));
    assert!(pending(&host, lead).await, "the report really landed");

    // The work is available again, and the member is marked gone.
    let plan = host.swarms().plan(&swarm()).await.unwrap();
    assert_eq!(ready_nodes(&plan).len(), 2);
    assert_eq!(
        host.swarms().member(&swarm(), worker).await.unwrap().status,
        MemberStatus::Departed
    );
}

#[tokio::test]
async fn a_task_that_keeps_outliving_its_workers_is_failed_rather_than_requeued_forever() {
    let sessions = Sessions::new();
    let (host, lead) = host_with(&sessions).await;

    for round in 0..=DEFAULT_RECLAIM_LIMIT {
        let worker = spawn(&host, lead, &format!("w{round}")).await;
        // Re-dispatch the plan's ready work to this round's worker, then kill it.
        let assignee = ActorId::new(worker.to_string());
        if round == 0 {
            plan_assigned_to(&host, lead, worker).await;
        } else {
            host.swarms()
                .with_plan(&swarm(), |plan| {
                    for node in ready_nodes(plan) {
                        dispatch(plan, node, &assignee).unwrap();
                    }
                })
                .await
                .unwrap();
        }
        salvage(&host, &swarm(), worker, DEFAULT_RECLAIM_LIMIT).await;
    }

    let plan = host.swarms().plan(&swarm()).await.unwrap();
    assert!(
        plan.nodes()
            .all(|node| matches!(node.status, TaskStatus::Failed { .. })),
        "after the reclaim budget the tasks fail loudly instead of cycling forever"
    );
    assert!(plan.is_settled(), "and the plan ends rather than hanging");
}

#[tokio::test]
async fn a_departed_coordinator_is_replaced_deterministically_and_its_children_reparented() {
    let sessions = Sessions::new();
    let (host, lead) = host_with(&sessions).await;
    let alpha = spawn(&host, lead, "alpha").await;
    let beta = spawn(&host, lead, "beta").await;
    let grandchild = spawn(&host, alpha, "alpha-child").await;

    let salvaged = salvage(&host, &swarm(), lead, DEFAULT_RECLAIM_LIMIT).await;

    // The successor is the lowest surviving id — the same answer for every
    // observer of the same state.
    let expected = [alpha, beta, grandchild]
        .into_iter()
        .min_by_key(SessionId::as_uuid)
        .unwrap();
    assert_eq!(salvaged.new_coordinator, Some(expected));
    assert_eq!(host.swarms().coordinator(&swarm()).await, Some(expected));

    // The coordinator's children were roots' worth of work; they now report to
    // the new coordinator, or are roots if there is nobody above them.
    let mut moved = salvaged.reparented.clone();
    moved.sort_by_key(SessionId::as_uuid);
    let mut expected_moved = vec![alpha, beta];
    expected_moved.sort_by_key(SessionId::as_uuid);
    assert_eq!(moved, expected_moved);

    // The tree is still walkable: nothing points at the departed member.
    for member in host.swarms().members(&swarm()).await {
        assert_ne!(
            member.report_back_to,
            Some(lead),
            "a dangling report-back edge is a completion report delivered nowhere"
        );
    }
}

#[tokio::test]
async fn a_sweep_finds_only_members_that_have_gone_quiet() {
    let sessions = Sessions::new();
    let (host, lead) = host_with(&sessions).await;
    let quiet = spawn(&host, lead, "quiet").await;
    let chatty = spawn(&host, lead, "chatty").await;

    let start = Instant::now();
    host.swarms().heartbeat_at(&swarm(), quiet, start).await;
    host.swarms().heartbeat_at(&swarm(), chatty, start).await;

    // Time passes; only `chatty` beats again.
    let later = start + Duration::from_secs(120);
    host.swarms()
        .heartbeat_at(&swarm(), chatty, later - Duration::from_secs(1))
        .await;

    let salvaged = sweep(&host, &swarm(), Duration::from_secs(90), 2, later).await;
    assert_eq!(salvaged.len(), 1);
    assert_eq!(salvaged[0].departed, quiet);
}

#[tokio::test]
async fn a_member_that_has_never_beaten_is_not_presumed_dead() {
    let sessions = Sessions::new();
    let (host, lead) = host_with(&sessions).await;
    let newborn = spawn(&host, lead, "newborn").await;

    // No heartbeat at all — a worker that has just been admitted and has not yet
    // had the chance. Reaping it here would reap every worker at birth.
    let salvaged = sweep(
        &host,
        &swarm(),
        Duration::from_secs(1),
        2,
        Instant::now() + Duration::from_secs(3600),
    )
    .await;
    assert!(salvaged.is_empty());
    assert_eq!(
        host.swarms()
            .member(&swarm(), newborn)
            .await
            .unwrap()
            .status,
        MemberStatus::Active
    );
}

#[tokio::test]
async fn the_reaper_releases_finished_members_but_keeps_what_they_reported() {
    let sessions = Sessions::new();
    let (host, lead) = host_with(&sessions).await;
    let worker = spawn(&host, lead, "alpha").await;

    host.swarms()
        .record_completion(&swarm(), worker, "found the bug")
        .await
        .unwrap();

    let reaped = reap_terminal(&host, &swarm()).await;
    assert_eq!(reaped, vec![worker]);
    assert!(host.host(worker).await.is_none(), "no longer hosted");
    assert_eq!(
        host.swarms()
            .member(&swarm(), worker)
            .await
            .unwrap()
            .completion
            .as_deref(),
        Some("found the bug"),
        "what it reported outlives the hosting"
    );
}

#[tokio::test]
async fn the_reaper_leaves_a_member_whose_children_still_report_to_it() {
    let sessions = Sessions::new();
    let (host, lead) = host_with(&sessions).await;
    let parent = spawn(&host, lead, "parent").await;
    let child = spawn(&host, parent, "child").await;

    host.swarms()
        .record_completion(&swarm(), parent, "my part is done")
        .await
        .unwrap();

    assert!(
        !reap_terminal(&host, &swarm()).await.contains(&parent),
        "reaping it would strand the child's report-back edge"
    );
    assert!(host.host(parent).await.is_some());
    let _ = child;
}

#[tokio::test]
async fn a_plan_and_its_membership_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let store = SnapshotStore::new(dir.path());
    let sessions = Sessions::new();

    // A swarm mid-flight: two members, a plan with work assigned.
    let (host, lead) = host_with(&sessions).await;
    let worker = spawn(&host, lead, "alpha").await;
    plan_assigned_to(&host, lead, worker).await;
    let before = host.swarms().plan(&swarm()).await.unwrap();
    store.capture(&host, &swarm()).await.unwrap();

    // The server restarts: a brand-new registry with nothing in it.
    let (restarted, _new_lead) = host_with(&Sessions::new()).await;
    let restarted = SwarmHost::new(
        restarted.sessions().clone(),
        SwarmRegistry::new(),
        Arc::clone(&sessions) as Arc<dyn WorkerFactory>,
    );
    assert!(restarted.swarms().plan(&swarm()).await.is_none());

    let snapshot = store.load(&swarm()).await.unwrap().unwrap();
    store.restore(&restarted, snapshot).await.unwrap();

    let after = restarted.swarms().plan(&swarm()).await.unwrap();
    assert_eq!(after, before, "the plan came back exactly");
    assert_eq!(after.version(), before.version());
    assert_eq!(restarted.swarms().coordinator(&swarm()).await, Some(lead));
    assert_eq!(restarted.swarms().members(&swarm()).await.len(), 2);
    assert_eq!(
        restarted.swarms().children(&swarm(), lead).await,
        vec![worker],
        "the spawn tree survived too"
    );

    // And the recovered plan is usable: salvaging the worker that is not coming
    // back returns its work to the pool.
    let salvaged = salvage(&restarted, &swarm(), worker, DEFAULT_RECLAIM_LIMIT).await;
    assert_eq!(salvaged.tasks.len(), 2);
    assert_eq!(
        ready_nodes(&restarted.swarms().plan(&swarm()).await.unwrap()).len(),
        2
    );
}

#[tokio::test]
async fn salvaging_twice_does_no_further_harm() {
    let sessions = Sessions::new();
    let (host, lead) = host_with(&sessions).await;
    let worker = spawn(&host, lead, "alpha").await;
    plan_assigned_to(&host, lead, worker).await;

    let first = salvage(&host, &swarm(), worker, DEFAULT_RECLAIM_LIMIT).await;
    let second = salvage(&host, &swarm(), worker, DEFAULT_RECLAIM_LIMIT).await;

    assert_eq!(first.tasks.len(), 2);
    assert!(
        second.tasks.is_empty(),
        "a racing second sweep must not reclaim the same work twice"
    );
    let plan = host.swarms().plan(&swarm()).await.unwrap();
    assert!(plan
        .nodes()
        .all(|node| node.reclaims == 1 && node.status == TaskStatus::Pending));
}
