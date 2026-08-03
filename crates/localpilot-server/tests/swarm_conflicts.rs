//! Two agents editing one file, end to end: real sessions, a real tool call
//! writing a real file, and the advisory alert that reaches the peer.
//!
//! This is the path that has to work: a tool reports what it touched, the turn
//! loop publishes it, the swarm host records it, and the peer is told. Any of
//! those four links can be present and the feature still be dead, which is why
//! this test drives the whole chain rather than the index in isolation.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use localpilot_core::SessionId;
use localpilot_harness::{SessionConfig, SessionRuntime, SoftInterrupt, SoftInterruptSource};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_server::registry::{RegistryError, SessionFactory, SessionRegistry};
use localpilot_server::swarm::registry::SwarmRegistry;
use localpilot_server::swarm::scope::SwarmId;
use localpilot_server::swarm::spawn::{
    AdoptedPair, SpawnRequest, Spawned, SwarmHost, WorkerFactory,
};
use localpilot_store::Store;
use tempfile::TempDir;

/// One shared workspace on disk — the whole point is that the agents are in the
/// same working tree.
struct Sessions {
    dir: Arc<TempDir>,
    script: std::sync::Mutex<Vec<FakeProvider>>,
}

impl Sessions {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            dir: Arc::new(tempfile::tempdir().unwrap()),
            script: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn root(&self) -> &std::path::Path {
        self.dir.path()
    }

    /// Queue the script the next built session will follow.
    fn queue(&self, provider: FakeProvider) {
        self.script
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(provider);
    }

    fn build(&self) -> Result<SessionRuntime, String> {
        let mut queued = self.script.lock().unwrap_or_else(|e| e.into_inner());
        let provider = if queued.is_empty() {
            FakeProvider::new().text("nothing to do")
        } else {
            queued.remove(0)
        };
        drop(queued);
        let root = self.root();
        let workspace = Workspace::new(root).map_err(|err| err.to_string())?;
        Ok(SessionRuntime::new(
            Arc::new(provider),
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
    SwarmId::new("conflict-swarm")
}

/// A provider script that edits `path`, replacing `find` with `replace`.
fn edits(path: &str, find: &str, replace: &str) -> FakeProvider {
    FakeProvider::new()
        .tool_call(
            "edit-1",
            "edit_file",
            serde_json::json!({
                "path": path,
                "old_text": find,
                "new_text": replace,
            }),
        )
        .text("edited")
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

async fn pending_interrupts(host: &SwarmHost, session: SessionId) -> Vec<SoftInterrupt> {
    host.sessions()
        .get(session)
        .await
        .unwrap()
        .lock()
        .await
        .steer_queue()
        .snapshot()
}

/// Twenty lines, so two edits can be near or far apart.
fn seed_file(root: &std::path::Path, name: &str) {
    let body: String = (1..=20)
        .map(|i| format!("line {i}\n"))
        .collect::<Vec<_>>()
        .concat();
    std::fs::write(root.join(name), body).unwrap();
}

/// A coordinator plus two workers sharing one working tree.
///
/// The two scripts are queued *after* the coordinator is built, because the
/// coordinator is a session too and would otherwise consume the first one — a
/// mistake that makes every assertion here pass or fail for the wrong reason.
async fn pair(
    sessions: &Arc<Sessions>,
    alice_script: FakeProvider,
    bob_script: FakeProvider,
) -> (SwarmHost, SessionId, SessionId, SessionId) {
    let registry = SessionRegistry::new();
    let lead = registry
        .open_new(&*(Arc::clone(sessions) as Arc<dyn SessionFactory>))
        .await
        .unwrap();
    let host = SwarmHost::new(
        registry,
        SwarmRegistry::new(),
        Arc::clone(sessions) as Arc<dyn WorkerFactory>,
    );
    host.adopt_root(&swarm(), lead, "lead").await.unwrap();

    sessions.queue(alice_script);
    let alice = spawn(&host, lead, "alice").await;
    sessions.queue(bob_script);
    let bob = spawn(&host, lead, "bob").await;
    (host, lead, alice, bob)
}

/// Two ordinary sessions adopted directly into the symmetric topology.
async fn symmetric_pair(
    sessions: &Arc<Sessions>,
    alice_script: FakeProvider,
    bob_script: FakeProvider,
) -> (SwarmHost, AdoptedPair) {
    let registry = SessionRegistry::new();
    let factory = Arc::clone(sessions) as Arc<dyn SessionFactory>;
    sessions.queue(alice_script);
    let alice = registry.open_new(&*factory).await.unwrap();
    sessions.queue(bob_script);
    let bob = registry.open_new(&*factory).await.unwrap();
    let host = SwarmHost::new(
        registry,
        SwarmRegistry::new(),
        Arc::clone(sessions) as Arc<dyn WorkerFactory>,
    );
    let pair = host
        .adopt_pair(&swarm(), (alice, "alice"), (bob, "bob"))
        .await
        .unwrap();
    (host, pair)
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

/// The touch reaches the index asynchronously (a broadcast subscriber), so wait
/// for it rather than racing it.
async fn wait_for<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..300 {
        if check().await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn two_agents_editing_the_same_lines_both_get_an_advisory_alert() {
    let sessions = Sessions::new();
    seed_file(sessions.root(), "shared.txt");
    // Overlapping blocks: alice rewrites lines 3-5, bob rewrites lines 4-6.
    let (host, _lead, alice, bob) = pair(
        &sessions,
        edits(
            "shared.txt",
            "line 3\nline 4\nline 5",
            "alice 3\nalice 4\nalice 5",
        ),
        edits(
            "shared.txt",
            "alice 4\nalice 5\nline 6",
            "bob 4\nbob 5\nbob 6",
        ),
    )
    .await;

    host.run(&swarm(), alice, "make your edit").await.unwrap();
    wait_for("alice's touch to be recorded", || async {
        !host.touches().is_empty(std::time::Instant::now()).await
    })
    .await;

    host.run(&swarm(), bob, "make your edit").await.unwrap();
    wait_for("both writers to be told", || async {
        pending(&host, alice).await && pending(&host, bob).await
    })
    .await;

    let alice_alerts = pending_interrupts(&host, alice).await;
    let bob_alerts = pending_interrupts(&host, bob).await;
    assert_advisory(&alice_alerts, "bob", "lines 4-6");
    assert_advisory(&bob_alerts, "alice", "lines 3-5");

    // Advisory only: both edits are on disk. Nothing was locked or rolled back,
    // which is the honest guarantee this mechanism makes.
    let content = std::fs::read_to_string(sessions.root().join("shared.txt")).unwrap();
    assert!(content.contains("alice 3"), "{content}");
    assert!(content.contains("bob 4"), "{content}");
}

#[tokio::test]
async fn an_adopted_pair_gets_symmetric_advisories_without_blocking_either_edit() {
    let sessions = Sessions::new();
    seed_file(sessions.root(), "shared.txt");
    let (host, pair) = symmetric_pair(
        &sessions,
        edits(
            "shared.txt",
            "line 3\nline 4\nline 5",
            "alice 3\nalice 4\nalice 5",
        ),
        edits(
            "shared.txt",
            "alice 4\nalice 5\nline 6",
            "bob 4\nbob 5\nbob 6",
        ),
    )
    .await;
    let [alice, bob] = pair.sessions();

    let alice_stop = pair.host(alice).unwrap().drive("make your edit").await;
    assert_eq!(alice_stop, localpilot_harness::StopReason::Done);
    wait_for("alice's pair touch to be recorded", || async {
        !host.touches().is_empty(std::time::Instant::now()).await
    })
    .await;

    let bob_stop = pair.host(bob).unwrap().drive("make your edit").await;
    assert_eq!(bob_stop, localpilot_harness::StopReason::Done);
    wait_for("both pair writers to be told", || async {
        pending(&host, alice).await && pending(&host, bob).await
    })
    .await;

    let alice_alerts = pending_interrupts(&host, alice).await;
    let bob_alerts = pending_interrupts(&host, bob).await;
    assert_advisory(&alice_alerts, "bob", "lines 4-6");
    assert_advisory(&bob_alerts, "alice", "lines 3-5");

    // Both successful tool results reached disk. The notices advise the peers;
    // they neither lock the workspace nor cancel or roll back either turn.
    let content = std::fs::read_to_string(sessions.root().join("shared.txt")).unwrap();
    assert!(content.contains("alice 3"), "{content}");
    assert!(content.contains("bob 4"), "{content}");
}

fn assert_advisory(alerts: &[SoftInterrupt], peer: &str, range: &str) {
    assert_eq!(alerts.len(), 1, "one advisory per colliding peer");
    let alert = &alerts[0];
    assert_eq!(alert.source, SoftInterruptSource::System);
    assert!(!alert.urgent, "advisories never cut a tool batch short");
    assert!(alert.content.contains(peer), "{}", alert.content);
    assert!(alert.content.contains(range), "{}", alert.content);
    assert!(
        alert
            .content
            .contains("Nothing was locked or rolled back — you both wrote"),
        "{}",
        alert.content
    );
    assert!(alert.content.contains("Re-read"), "{}", alert.content);
}

#[tokio::test]
async fn non_overlapping_edits_in_an_adopted_pair_remain_quiet() {
    let sessions = Sessions::new();
    seed_file(sessions.root(), "shared.txt");
    let (host, pair) = symmetric_pair(
        &sessions,
        edits("shared.txt", "line 2\nline 3", "alice 2\nline 3"),
        edits("shared.txt", "line 18\nline 19", "line 18\nbob 19"),
    )
    .await;
    let [alice, bob] = pair.sessions();

    assert_eq!(
        pair.host(alice).unwrap().drive("edit the top").await,
        localpilot_harness::StopReason::Done
    );
    wait_for("alice's pair touch to be recorded", || async {
        !host.touches().is_empty(std::time::Instant::now()).await
    })
    .await;
    assert_eq!(
        pair.host(bob).unwrap().drive("edit the bottom").await,
        localpilot_harness::StopReason::Done
    );

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(!pending(&host, alice).await);
    assert!(!pending(&host, bob).await);
    let content = std::fs::read_to_string(sessions.root().join("shared.txt")).unwrap();
    assert!(content.contains("alice 2"), "{content}");
    assert!(content.contains("bob 19"), "{content}");
}

#[tokio::test]
async fn edits_far_apart_in_one_file_do_not_interrupt_anyone() {
    let sessions = Sessions::new();
    seed_file(sessions.root(), "shared.txt");
    let (host, _lead, alice, bob) = pair(
        &sessions,
        // Anchored on two lines each: a bare "line 2" also matches inside
        // "line 20", and an ambiguous anchor fails the edit rather than making
        // it — which would make this test pass for the wrong reason.
        edits(
            "shared.txt",
            "line 2
line 3",
            "alice 2
line 3",
        ),
        edits(
            "shared.txt",
            "line 18
line 19",
            "line 18
bob 19",
        ),
    )
    .await;

    host.run(&swarm(), alice, "edit the top").await.unwrap();
    wait_for("alice's touch to be recorded", || async {
        !host.touches().is_empty(std::time::Instant::now()).await
    })
    .await;
    host.run(&swarm(), bob, "edit the bottom").await.unwrap();

    // Give any alert that was going to fire the chance to.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(
        !pending(&host, alice).await,
        "two agents working in different parts of one file is ordinary, and interrupting them \
         about it would make the mechanism the problem"
    );
    assert!(!pending(&host, bob).await);
}

#[tokio::test]
async fn an_edit_to_a_different_file_reaches_nobody() {
    let sessions = Sessions::new();
    seed_file(sessions.root(), "a.txt");
    seed_file(sessions.root(), "b.txt");
    let (host, _lead, alice, bob) = pair(
        &sessions,
        edits("a.txt", "line 3", "line 3 (alice)"),
        edits("b.txt", "line 3", "line 3 (bob)"),
    )
    .await;
    host.run(&swarm(), alice, "edit a").await.unwrap();
    wait_for("alice's touch to be recorded", || async {
        !host.touches().is_empty(std::time::Instant::now()).await
    })
    .await;
    host.run(&swarm(), bob, "edit b").await.unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(!pending(&host, alice).await);
    assert!(!pending(&host, bob).await);
}

#[tokio::test]
async fn a_prior_reader_is_told_when_the_ground_moves_under_it() {
    let sessions = Sessions::new();
    seed_file(sessions.root(), "shared.txt");
    let (host, _lead, alice, bob) = pair(
        &sessions,
        FakeProvider::new()
            .tool_call(
                "read-1",
                "read_file",
                serde_json::json!({ "path": "shared.txt" }),
            )
            .text("read it"),
        edits("shared.txt", "line 3", "line 3 (bob)"),
    )
    .await;

    host.run(&swarm(), alice, "read the file").await.unwrap();
    wait_for("alice's read to be recorded", || async {
        !host.touches().is_empty(std::time::Instant::now()).await
    })
    .await;

    host.run(&swarm(), bob, "edit the file").await.unwrap();
    wait_for("the reader to be told", || pending(&host, alice)).await;
    assert!(
        !pending(&host, bob).await,
        "a writer is not interrupted merely because a peer had read the file"
    );
}
