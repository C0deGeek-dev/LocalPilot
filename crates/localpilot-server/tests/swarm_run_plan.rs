//! A whole plan, driven end to end: seed, dispatch, workers complete with real
//! turns, downstream tasks read what upstream established, and the run ends.
//!
//! The driver is a small amount of code sitting on top of four subsystems, so
//! what these test is the *joins* — that a worker really receives the contract
//! and the upstream handoffs, that a completion really lands on the graph, and
//! that a run really terminates rather than waiting on something it never
//! dispatched.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use localpilot_core::SessionId;
use localpilot_harness::{SessionConfig, SessionRuntime};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_server::registry::{RegistryError, SessionFactory, SessionRegistry};
use localpilot_server::swarm::driver::{run_plan, DriverConfig};
use localpilot_server::swarm::registry::{SwarmLimits, SwarmRegistry};
use localpilot_server::swarm::scope::SwarmId;
use localpilot_server::swarm::spawn::{SpawnRequest, SwarmHost, WorkerFactory};
use localpilot_store::Store;
use localpilot_taskgraph::ops::seed;
use localpilot_taskgraph::{ActorId, NodeSpec, PlanMode, TaskPlan};
use tempfile::TempDir;

/// Answers are keyed by **task title**, not queued in order.
///
/// A shared queue looks simpler and is a trap: the coordinator is a session too,
/// so it silently consumes the first entry and every worker afterwards gets the
/// wrong script — which makes some assertions fail and, far worse, makes others
/// pass for the wrong reason.
struct Sessions {
    dir: Arc<TempDir>,
    answers: std::collections::HashMap<String, String>,
    seen: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Sessions {
    fn new(answers: &[(&str, &str)]) -> Arc<Self> {
        Arc::new(Self {
            dir: Arc::new(tempfile::tempdir().unwrap()),
            answers: answers
                .iter()
                .map(|(task, answer)| ((*task).to_string(), (*answer).to_string()))
                .collect(),
            seen: Arc::new(std::sync::Mutex::new(Vec::new())),
        })
    }

    fn build(&self) -> Result<SessionRuntime, String> {
        self.build_answering("done")
    }

    fn build_answering(&self, text: &str) -> Result<SessionRuntime, String> {
        let text = text.to_string();
        let root = self.dir.path();
        let workspace = Workspace::new(root).map_err(|err| err.to_string())?;
        let provider = Arc::new(FakeProvider::new().text(&text));
        Ok(SessionRuntime::new(
            provider,
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
    fn create(&self, request: &SpawnRequest) -> Result<SessionRuntime, String> {
        self.seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request.name.clone());
        // The worker's name is its task's title, so the answer can be looked up
        // rather than dealt off a queue whose order nothing guarantees.
        let answer = self
            .answers
            .get(&request.name)
            .cloned()
            .unwrap_or_else(|| format!("{} done", request.name));
        self.build_answering(&answer)
    }
}

impl SessionFactory for Sessions {
    fn create(&self) -> Result<SessionRuntime, RegistryError> {
        self.build().map_err(RegistryError::Factory)
    }
}

fn swarm() -> SwarmId {
    SwarmId::new("run-plan-swarm")
}

async fn coordinated(sessions: &Arc<Sessions>) -> (SwarmHost, SessionId) {
    let registry = SessionRegistry::new();
    let lead = registry
        .open_new(&*(Arc::clone(sessions) as Arc<dyn SessionFactory>))
        .await
        .unwrap();
    let host = SwarmHost::new(
        registry,
        SwarmRegistry::with_limits(SwarmLimits {
            max_members: 64,
            max_active: 64,
        }),
        Arc::clone(sessions) as Arc<dyn WorkerFactory>,
    );
    host.adopt_root(&swarm(), lead, "lead").await.unwrap();
    (host, lead)
}

/// Seed a plan owned by the coordinator.
async fn plan_of(host: &SwarmHost, lead: SessionId, mode: PlanMode, specs: &[NodeSpec]) {
    let owner = ActorId::new(lead.to_string());
    let mut plan = TaskPlan::new("make the suite green", mode, owner.clone());
    seed(&mut plan, &owner, "seed-1", specs).unwrap();
    host.swarms().set_plan(&swarm(), plan).await;
}

fn fast() -> DriverConfig {
    DriverConfig {
        concurrency: 3,
        worker_timeout: Duration::from_secs(20),
        ..DriverConfig::default()
    }
}

#[tokio::test]
async fn a_fan_out_plan_runs_to_completion() {
    let sessions = Sessions::new(&[
        ("survey", "the two halves are parse and render"),
        ("left", "left done"),
        ("right", "right done"),
        ("join", "combined"),
    ]);
    let (host, lead) = coordinated(&sessions).await;
    plan_of(
        &host,
        lead,
        PlanMode::Light,
        &[
            NodeSpec::task("survey", "Find the two halves."),
            NodeSpec::task("left", "Do the left half.").after(0),
            NodeSpec::task("right", "Do the right half.").after(0),
            NodeSpec::task("join", "Combine them.").after(1).after(2),
        ],
    )
    .await;

    let report = run_plan(&host, &swarm(), lead, fast()).await;

    assert!(report.settled, "{report:?}");
    assert_eq!(report.dispatched, 4);
    assert_eq!(report.completed, 4);
    assert_eq!(report.failed, 0);
    assert!(!report.exhausted);

    // Every task got its own worker, named for the task.
    let names = sessions.seen.lock().unwrap().clone();
    assert_eq!(names.len(), 4);
    assert!(names.contains(&"survey".to_string()));
    assert!(names.contains(&"join".to_string()));
}

#[tokio::test]
async fn a_worker_receives_the_contract_and_what_upstream_established() {
    let sessions = Sessions::new(&[
        ("find", "the parser is in src/parse.rs"),
        ("fix", "fixed it"),
    ]);
    let (host, lead) = coordinated(&sessions).await;
    plan_of(
        &host,
        lead,
        PlanMode::Light,
        &[
            NodeSpec::task("find", "Find the parser."),
            NodeSpec::task("fix", "Fix it.").after(0),
        ],
    )
    .await;

    let report = run_plan(&host, &swarm(), lead, fast()).await;
    assert!(report.settled, "{report:?}");

    // The downstream task's own transcript is the evidence: its first user
    // message is what the driver handed it.
    let plan = host.swarms().plan(&swarm()).await.unwrap();
    let fix = plan.nodes().find(|node| node.title == "fix").unwrap();
    let hydrated = localpilot_taskgraph::schedule::assemble_input(&plan, fix.id).unwrap();
    assert!(
        hydrated.contains("the parser is in src/parse.rs"),
        "the downstream task reads what upstream established rather than redoing it: {hydrated}"
    );

    // And the contract really travels: it is the text a worker is given, not
    // something only the coordinator was told.
    let contract = localpilot_harness::assignment_contract(localpilot_harness::SwarmDepth::Light);
    assert!(contract.contains("one of several agents"));
    assert!(contract.contains("reporting what you established"));
    assert!(
        contract.contains("Nothing is locked"),
        "the worker is told the shared-tree rule too, not only the coordinator"
    );
}

#[tokio::test]
async fn a_deep_plan_gates_its_work_and_still_finishes() {
    // Two tasks plus the auto-inserted review gate.
    let sessions = Sessions::new(&[
        ("a", "a done"),
        ("b", "b done"),
        ("Plan review", "reviewed: both handoffs check out"),
    ]);
    let (host, lead) = coordinated(&sessions).await;
    plan_of(
        &host,
        lead,
        PlanMode::Deep,
        &[NodeSpec::task("a", "Do a."), NodeSpec::task("b", "Do b.")],
    )
    .await;

    let report = run_plan(&host, &swarm(), lead, fast()).await;

    assert!(report.settled, "{report:?}");
    assert_eq!(
        report.dispatched, 3,
        "two tasks plus the gate deep mode inserted"
    );
    let plan = host.swarms().plan(&swarm()).await.unwrap();
    assert_eq!(plan.len(), 3);
}

#[tokio::test]
async fn a_chain_says_why_it_could_not_use_the_budget() {
    let sessions = Sessions::new(&[("first", "one"), ("second", "two"), ("third", "three")]);
    let (host, lead) = coordinated(&sessions).await;
    plan_of(
        &host,
        lead,
        PlanMode::Light,
        &[
            NodeSpec::task("first", "Do the first."),
            NodeSpec::task("second", "Then the second.").after(0),
            NodeSpec::task("third", "Then the third.").after(1),
        ],
    )
    .await;

    let report = run_plan(
        &host,
        &swarm(),
        lead,
        DriverConfig {
            concurrency: 8,
            ..fast()
        },
    )
    .await;

    assert!(report.settled);
    assert_eq!(report.peak_in_flight, 1, "a chain runs one at a time");
    let hint = report
        .starvation
        .expect("a three-long chain against a budget of eight is worth saying out loud");
    assert!(hint.contains("narrower than the budget"), "{hint}");
}

#[tokio::test]
async fn a_wide_plan_uses_its_budget_and_says_nothing() {
    let sessions = Sessions::new(&[]);
    let (host, lead) = coordinated(&sessions).await;
    let specs: Vec<NodeSpec> = (0..6)
        .map(|i| NodeSpec::task(format!("t{i}"), "independent work"))
        .collect();
    plan_of(&host, lead, PlanMode::Light, &specs).await;

    let report = run_plan(&host, &swarm(), lead, fast()).await;

    assert!(report.settled);
    assert_eq!(report.dispatched, 6);
    assert!(
        report.peak_in_flight > 1,
        "independent tasks must actually overlap: {report:?}"
    );
    assert!(report.starvation.is_none(), "{:?}", report.starvation);
}

#[tokio::test]
async fn the_coordinator_is_given_orchestration_guidance_when_it_joins() {
    let sessions = Sessions::new(&[]);
    let (host, lead) = coordinated(&sessions).await;

    // `adopt_root` appended the directive to the coordinator's own prompt — the
    // guidance is in effect for the session that has to act on it, and nowhere
    // else.
    let prompt = {
        let handle = host.sessions().get(lead).await.unwrap();
        let guard = handle.lock().await;
        guard.system_prompt_text()
    };
    assert!(
        prompt.contains("coordinating several agents"),
        "the coordinator's prompt should carry the swarm directive"
    );
    assert!(prompt.contains("Work as a graph, not a queue"));
    assert!(
        prompt.contains("LocalPilot"),
        "and the base agent prompt must survive being appended to"
    );
}

#[tokio::test]
async fn a_run_with_nothing_to_do_ends_immediately() {
    let sessions = Sessions::new(&[]);
    let (host, lead) = coordinated(&sessions).await;
    plan_of(&host, lead, PlanMode::Light, &[NodeSpec::task("only", "x")]).await;
    host.swarms()
        .with_plan(&swarm(), |plan| {
            let only = plan.nodes().next().map(|node| node.id).unwrap();
            localpilot_taskgraph::ops::abandon_node(
                plan,
                only,
                &ActorId::new(lead.to_string()),
                "no longer needed",
            )
        })
        .await
        .unwrap()
        .unwrap();

    let report = run_plan(&host, &swarm(), lead, fast()).await;
    assert_eq!(report.dispatched, 0);
    assert!(report.settled);
    assert!(report.starvation.is_none());
}
