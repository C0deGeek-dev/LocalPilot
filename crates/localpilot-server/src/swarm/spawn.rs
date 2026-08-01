//! Starting workers, running them in parallel, and getting their answers back.
//!
//! A swarm worker is an **ordinary hosted session with a swarm edge** — not a
//! second process, not a second loop, and not a special case anywhere in the
//! session path. That is what makes the rest of the server work on it unchanged:
//! the registry hosts it, a [`SessionHost`] drives it, cancel and steer reach it,
//! and the reaper can clean it up.
//!
//! What this module adds is the three things a worker needs that a session does
//! not:
//!
//! - **A slot, taken before the expensive part.** Building a session is slow, so
//!   the caps are enforced by reserving first and confirming after (see
//!   [`SwarmRegistry::reserve`]). A spawn that fails to build gives its slot
//!   back.
//! - **A checked model.** A spawn may ask for a specific model. If the built
//!   session is not on it, the spawn is *refused*. Running anyway is the worst
//!   available outcome: the work completes, the report reads normally, and
//!   nothing says the wrong model produced it.
//! - **A way home.** When a worker's turn ends, its answer is captured, bounded,
//!   recorded on its membership, and injected into whoever it reports back to —
//!   as a labelled background message, so the coordinator can tell a worker's
//!   report from something its user typed.
//!
//! Building the session itself is deliberately *not* here. Narrowing tools to
//! the parent's, forwarding permission asks to the parent's approver, resolving a
//! provider — all of that needs the wiring the CLI owns, and putting it behind
//! [`WorkerFactory`] keeps this crate off that path and lets the tests drive the
//! whole lifecycle with a session that has no provider at all.

use std::collections::HashMap;
use std::sync::Arc;

use localpilot_core::SessionId;
use localpilot_harness::{
    agent_run::bound_summary, SessionRuntime, SoftInterrupt, SoftInterruptSource, StopReason,
};
use tokio::sync::RwLock;

use super::registry::{Admission, MemberStatus, SwarmError, SwarmMember, SwarmRegistry};
use super::scope::SwarmId;
use crate::host::SessionHost;
use crate::registry::{RegistryError, SessionRegistry};

/// How much of a worker's answer travels back to its spawner.
///
/// The point of delegating is that the caller's context stays clean, so a worker
/// that returned everything it read would be worse than no worker at all. The
/// same bound the in-process delegation path uses.
const MAX_REPORT_BYTES: usize = 4 * 1024;

/// What to spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRequest {
    /// The swarm to join.
    pub swarm: SwarmId,
    /// Who is spawning, and therefore who the worker reports back to.
    pub parent: SessionId,
    /// A short name peers can address it by.
    pub name: String,
    /// The work, as the first thing the worker is told.
    pub task: String,
    /// Makes a retried spawn idempotent. Strongly recommended: without it, a
    /// caller that retried after a lost response starts a second worker on the
    /// same task, and the two then edit the same files.
    pub idempotency_key: Option<String>,
    /// Require this model. `None` accepts whatever the factory builds.
    pub model: Option<String>,
}

impl SpawnRequest {
    /// A spawn with no model requirement and no idempotency key.
    #[must_use]
    pub fn new(
        swarm: SwarmId,
        parent: SessionId,
        name: impl Into<String>,
        task: impl Into<String>,
    ) -> Self {
        Self {
            swarm,
            parent,
            name: name.into(),
            task: task.into(),
            idempotency_key: None,
            model: None,
        }
    }

    /// Make this spawn idempotent under `key`.
    #[must_use]
    pub fn with_key(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self
    }

    /// Require a specific model.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }
}

/// Builds the session a worker runs in.
///
/// The host implements this, exactly as it does for delegation and background
/// processes: a server crate has no provider, no tool registry, and no approver,
/// so it cannot build a session and should not learn how.
pub trait WorkerFactory: Send + Sync {
    /// Build a fresh session for `request`.
    ///
    /// The implementation owns containment: narrowing the worker's tools to a
    /// subset of the spawner's, attributing its permission asks to the spawner's
    /// approver, and honouring `request.model` if it can. This module verifies
    /// the model afterwards rather than trusting it.
    ///
    /// # Errors
    /// Any human-readable reason the session could not be built.
    fn create(&self, request: &SpawnRequest) -> Result<SessionRuntime, String>;
}

/// What a spawn resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Spawned {
    /// A worker was started.
    Started {
        /// Its session.
        session: SessionId,
    },
    /// This idempotency key is already being spawned. Nothing was started.
    AlreadyStarting,
    /// This idempotency key already produced a worker.
    Already {
        /// The worker the first attempt produced.
        session: SessionId,
    },
}

/// Why a spawn was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SpawnError {
    /// The swarm's caps refused it.
    #[error(transparent)]
    Admission(#[from] SwarmError),
    /// The factory could not build a session.
    #[error("could not start a worker: {0}")]
    Factory(String),
    /// A model was asked for and a different one was built.
    #[error(
        "asked for model {requested:?} but the worker would run on {actual:?} — refusing rather \
         than producing work that silently came from the wrong model"
    )]
    ModelMismatch {
        /// What the spawn asked for.
        requested: String,
        /// What it would have got.
        actual: String,
    },
    /// The session registry refused the new session.
    #[error("could not register the worker: {0}")]
    Registry(String),
}

impl From<RegistryError> for SpawnError {
    fn from(error: RegistryError) -> Self {
        SpawnError::Registry(error.to_string())
    }
}

/// What a finished worker reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerReport {
    /// The worker.
    pub session: SessionId,
    /// Its name, for a human-readable notification.
    pub name: String,
    /// Its final answer, bounded.
    pub summary: String,
    /// Whether the answer was cut to fit.
    pub truncated: bool,
    /// Why its turn ended.
    pub stop: StopReason,
    /// Who it was delivered to, if that session is still hosted here.
    pub delivered_to: Option<SessionId>,
}

/// The swarm's side of the server: session hosting plus swarm membership, and
/// the spawn path that joins them.
///
/// Cloning is cheap and yields another handle onto the same state.
#[derive(Clone)]
pub struct SwarmHost {
    sessions: SessionRegistry,
    swarms: SwarmRegistry,
    /// One host per session, so a report can be delivered to a coordinator whose
    /// turn is already running. Kept here rather than in the session registry
    /// because a host is a *serving* concern: a session hosted for nobody needs
    /// none.
    hosts: Arc<RwLock<HashMap<SessionId, Arc<SessionHost>>>>,
    factory: Arc<dyn WorkerFactory>,
    /// Who touched which file recently. Shared across the swarm, because the
    /// whole point is that one member's edit is visible to another.
    touches: super::touches::TouchIndex,
}

impl SwarmHost {
    /// Build a swarm host over an existing session registry.
    #[must_use]
    pub fn new(
        sessions: SessionRegistry,
        swarms: SwarmRegistry,
        factory: Arc<dyn WorkerFactory>,
    ) -> Self {
        Self {
            sessions,
            swarms,
            hosts: Arc::new(RwLock::new(HashMap::new())),
            factory,
            touches: super::touches::TouchIndex::default(),
        }
    }

    /// The shared record of who touched what recently.
    #[must_use]
    pub fn touches(&self) -> &super::touches::TouchIndex {
        &self.touches
    }

    /// The swarm membership registry.
    #[must_use]
    pub fn swarms(&self) -> &SwarmRegistry {
        &self.swarms
    }

    /// The session registry.
    #[must_use]
    pub fn sessions(&self) -> &SessionRegistry {
        &self.sessions
    }

    /// Adopt an already-registered session as a swarm root, hosting it so
    /// reports can reach it.
    ///
    /// This is how a coordinator joins: it is an ordinary session that already
    /// exists, and it needs a host of its own before anything it spawns can
    /// report home.
    ///
    /// # Errors
    /// [`SpawnError::Registry`] if no such session is registered, or
    /// [`SpawnError::Admission`] if the swarm's caps refuse another member.
    pub async fn adopt_root(
        &self,
        swarm: &SwarmId,
        session: SessionId,
        name: impl Into<String>,
    ) -> Result<Arc<SessionHost>, SpawnError> {
        let handle = self
            .sessions
            .get(session)
            .await
            .ok_or_else(|| SpawnError::Registry(RegistryError::NotFound.to_string()))?;
        self.swarms.join_as_root(swarm, session, name).await?;
        // A session that has just joined a swarm needs orchestration guidance it
        // did not need a moment ago. Appended rather than replacing, so whatever
        // else the host put in the prompt survives.
        {
            let depth = match self.swarms.plan(swarm).await.map(|plan| plan.mode()) {
                Some(localpilot_taskgraph::PlanMode::Deep) => localpilot_harness::SwarmDepth::Deep,
                _ => localpilot_harness::SwarmDepth::Light,
            };
            handle
                .lock()
                .await
                .append_system_prompt(localpilot_harness::swarm_coordinator_directive(depth));
        }
        let host = self.host_for(session, handle).await;
        self.bind_peers(swarm, session).await;
        self.watch_touches(swarm, session, &host);
        Ok(host)
    }

    /// The host for a session, if this swarm host is serving it.
    pub async fn host(&self, session: SessionId) -> Option<Arc<SessionHost>> {
        self.hosts.read().await.get(&session).cloned()
    }

    /// Start a worker. Does **not** run its turn — see [`run`](Self::run) and
    /// [`dispatch`](Self::dispatch).
    ///
    /// The order matters and is not the obvious one: reserve a slot, build,
    /// check the model, register the session, *then* confirm the member. Every
    /// early exit after the reservation releases it, so a failed spawn does not
    /// leak a slot for the life of the server.
    ///
    /// # Errors
    /// [`SpawnError::Admission`], [`SpawnError::Factory`],
    /// [`SpawnError::ModelMismatch`], or [`SpawnError::Registry`].
    pub async fn spawn(&self, request: &SpawnRequest) -> Result<Spawned, SpawnError> {
        let reservation = match self
            .swarms
            .reserve(&request.swarm, request.idempotency_key.as_deref())
            .await?
        {
            Admission::Reserved(slot) => slot,
            Admission::InFlight => return Ok(Spawned::AlreadyStarting),
            Admission::Existing(session) => return Ok(Spawned::Already { session }),
        };

        let runtime = match self.factory.create(request) {
            Ok(runtime) => runtime,
            Err(reason) => {
                self.swarms.release(reservation).await;
                return Err(SpawnError::Factory(reason));
            }
        };
        if let Some(requested) = &request.model {
            let actual = runtime.model();
            if actual != requested {
                let actual = actual.to_string();
                self.swarms.release(reservation).await;
                return Err(SpawnError::ModelMismatch {
                    requested: requested.clone(),
                    actual,
                });
            }
        }

        let session = runtime.session_id();
        if let Err(error) = self.sessions.register(runtime).await {
            self.swarms.release(reservation).await;
            return Err(error.into());
        }
        let Some(handle) = self.sessions.get(session).await else {
            self.swarms.release(reservation).await;
            return Err(SpawnError::Registry(RegistryError::NotFound.to_string()));
        };
        self.swarms
            .confirm(
                reservation,
                SwarmMember::worker(session, request.name.clone(), request.parent),
            )
            .await?;
        let host = self.host_for(session, handle).await;
        self.bind_peers(&request.swarm, session).await;
        self.watch_touches(&request.swarm, session, &host);
        Ok(Spawned::Started { session })
    }

    /// Run one worker's turn on `task` and flow its answer back to whoever it
    /// reports to.
    ///
    /// A turn that ends for any reason is still a completion: "the worker gave
    /// up" is information the spawner should reason about, not an error to
    /// swallow. The recorded status is what distinguishes the two.
    ///
    /// # Errors
    /// [`SpawnError::Registry`] if the worker is not hosted here.
    pub async fn run(
        &self,
        swarm: &SwarmId,
        session: SessionId,
        task: &str,
    ) -> Result<WorkerReport, SpawnError> {
        let host = self
            .host(session)
            .await
            .ok_or_else(|| SpawnError::Registry(RegistryError::NotFound.to_string()))?;
        let stop = host.drive(task).await;
        self.finish(swarm, session, stop).await
    }

    /// Start a worker and run it, returning as soon as it is running.
    ///
    /// The returned handle resolves to the worker's report. Several of these
    /// awaited together is what "parallel workers" means here — the concurrency
    /// budget is already enforced at [`spawn`](Self::spawn), so a caller cannot
    /// start more than the swarm allows however many it asks for.
    ///
    /// # Errors
    /// Anything [`spawn`](Self::spawn) returns.
    pub async fn dispatch(
        &self,
        request: SpawnRequest,
    ) -> Result<
        (
            SessionId,
            tokio::task::JoinHandle<Result<WorkerReport, SpawnError>>,
        ),
        SpawnError,
    > {
        let session = match self.spawn(&request).await? {
            Spawned::Started { session } => session,
            Spawned::Already { session } => session,
            Spawned::AlreadyStarting => {
                return Err(SpawnError::Admission(SwarmError::StaleReservation))
            }
        };
        let host = self
            .host(session)
            .await
            .ok_or_else(|| SpawnError::Registry(RegistryError::NotFound.to_string()))?;
        let this = self.clone();
        let swarm = request.swarm.clone();
        let task = request.task.clone();
        let handle = tokio::spawn(async move {
            let stop = host.drive(&task).await;
            this.finish(&swarm, session, stop).await
        });
        Ok((session, handle))
    }

    /// Capture a finished worker's answer, record it, and deliver it home.
    async fn finish(
        &self,
        swarm: &SwarmId,
        session: SessionId,
        stop: StopReason,
    ) -> Result<WorkerReport, SpawnError> {
        let raw = match self.sessions.get(session).await {
            Some(handle) => handle
                .lock()
                .await
                .last_assistant_text()
                .unwrap_or_default(),
            None => String::new(),
        };
        let (summary, truncated) = bound_summary(&raw, MAX_REPORT_BYTES);
        let name = self
            .swarms
            .member(swarm, session)
            .await
            .map_or_else(String::new, |member| member.name);

        let owner = self
            .swarms
            .record_completion(swarm, session, summary.clone())
            .await
            .ok()
            .flatten();

        let delivered_to = match owner {
            Some(owner) => match self.host(owner).await {
                Some(host) => {
                    host.inject(SoftInterrupt {
                        content: report_message(&name, session, &summary, truncated),
                        source: SoftInterruptSource::BackgroundTask,
                        urgent: false,
                    });
                    Some(owner)
                }
                // The spawner is gone. The report is still recorded on the
                // member, so a re-elected coordinator can read it; it simply has
                // nowhere to be delivered right now.
                None => None,
            },
            None => None,
        };

        Ok(WorkerReport {
            session,
            name,
            summary,
            truncated,
            stop,
            delivered_to,
        })
    }

    /// Mark a worker as having failed, so its slot and its assignments can be
    /// dealt with.
    ///
    /// # Errors
    /// [`SpawnError::Admission`] if the member is unknown.
    pub async fn mark_failed(
        &self,
        swarm: &SwarmId,
        session: SessionId,
        reason: impl Into<String>,
    ) -> Result<(), SpawnError> {
        self.swarms
            .set_status(
                swarm,
                session,
                MemberStatus::Failed {
                    reason: reason.into(),
                },
            )
            .await?;
        Ok(())
    }

    /// Stop hosting a session. Its membership record is left alone: the failure
    /// lifecycle decides what a departed member means, not the fact that nobody
    /// is serving it any more.
    pub async fn unhost(&self, session: SessionId) -> Option<Arc<SessionHost>> {
        // Its touches go with it: a departed agent should not keep generating
        // alerts about files nobody is holding.
        self.touches.forget(session).await;
        self.hosts.write().await.remove(&session)
    }

    /// Watch `session`'s event stream for file touches and turn them into
    /// advisory alerts to the peers they affect.
    ///
    /// A subscriber rather than a hook in the tool path: the tool that changed a
    /// file has no business knowing a swarm exists, and this way a session
    /// outside one costs nothing at all.
    fn watch_touches(&self, swarm: &SwarmId, session: SessionId, host: &Arc<SessionHost>) {
        let mut events = host.subscribe();
        let this = self.clone();
        let swarm = swarm.clone();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(localpilot_harness::RuntimeEvent::FilesTouched(touched)) => {
                        for touch in &touched {
                            super::touches::announce(&this, &this.touches, &swarm, session, touch)
                                .await;
                        }
                    }
                    Ok(_) => {}
                    // Lagging is survivable — a missed alert, not a broken
                    // session — so resynchronise rather than give up watching.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    /// Give `session` its view of the swarm, so the messaging tool it already
    /// carries stops reporting itself unavailable.
    ///
    /// Done here rather than in the factory because the view has to name the
    /// session, and the session id does not exist until the runtime is built.
    async fn bind_peers(&self, swarm: &SwarmId, session: SessionId) {
        if let Some(handle) = self.sessions.get(session).await {
            let peers = super::messaging::SessionPeers::new(self.clone(), swarm.clone(), session);
            handle.lock().await.set_peers(Arc::new(peers));
        }
    }

    /// The host for `session`, creating it if this is the first time.
    async fn host_for(
        &self,
        session: SessionId,
        handle: crate::registry::SessionHandle,
    ) -> Arc<SessionHost> {
        if let Some(existing) = self.hosts.read().await.get(&session) {
            return Arc::clone(existing);
        }
        let host = Arc::new(SessionHost::new(handle).await);
        let mut guard = self.hosts.write().await;
        // Another task may have raced us here; keep whichever landed first so
        // every holder of a host for this session holds the *same* one — two
        // hosts would mean two turn-token slots and a cancel that reaches
        // neither.
        Arc::clone(guard.entry(session).or_insert(host))
    }
}

/// The message a spawner sees when one of its workers finishes.
///
/// Names the worker and says plainly that it is finished, because the reader is
/// a model mid-turn that has to decide whether this changes what it is doing.
fn report_message(name: &str, session: SessionId, summary: &str, truncated: bool) -> String {
    let who = if name.trim().is_empty() {
        session.to_string()
    } else {
        format!("{name} ({session})")
    };
    let body = if summary.trim().is_empty() {
        "It finished without reporting anything.".to_string()
    } else {
        summary.to_string()
    };
    let note = if truncated {
        "\n[report truncated]"
    } else {
        ""
    };
    format!("Worker {who} finished.\n\n{body}{note}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swarm::registry::SwarmLimits;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// What a test's stand-in factory does when asked to build.
    type Build = dyn Fn(&SpawnRequest) -> Result<SessionRuntime, String> + Send + Sync;

    /// A factory over a caller-supplied builder, so each test decides what a
    /// worker is without a provider anywhere in sight.
    struct TestFactory {
        build: Box<Build>,
        calls: AtomicUsize,
    }

    impl TestFactory {
        fn new(
            build: impl Fn(&SpawnRequest) -> Result<SessionRuntime, String> + Send + Sync + 'static,
        ) -> Arc<Self> {
            Arc::new(Self {
                build: Box::new(build),
                calls: AtomicUsize::new(0),
            })
        }
    }

    impl WorkerFactory for TestFactory {
        fn create(&self, request: &SpawnRequest) -> Result<SessionRuntime, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            (self.build)(request)
        }
    }

    fn swarm() -> SwarmId {
        SwarmId::new("test-swarm")
    }

    #[tokio::test]
    async fn a_factory_failure_gives_the_slot_back() {
        let factory = TestFactory::new(|_| Err("no provider configured".into()));
        let host = SwarmHost::new(
            SessionRegistry::new(),
            SwarmRegistry::with_limits(SwarmLimits {
                max_members: 8,
                max_active: 1,
            }),
            Arc::clone(&factory) as Arc<dyn WorkerFactory>,
        );

        let request = SpawnRequest::new(swarm(), SessionId::new(), "w", "do it");
        assert!(matches!(
            host.spawn(&request).await,
            Err(SpawnError::Factory(_))
        ));
        // The single concurrency slot is free again, so a second attempt gets as
        // far as the factory rather than being refused by the cap.
        assert!(matches!(
            host.spawn(&request).await,
            Err(SpawnError::Factory(_))
        ));
        assert_eq!(factory.calls.load(Ordering::SeqCst), 2);
        assert!(host.swarms().members(&swarm()).await.is_empty());
    }

    #[tokio::test]
    async fn the_cap_still_refuses_before_the_factory_is_asked() {
        let factory = TestFactory::new(|_| Err("unreachable".into()));
        let host = SwarmHost::new(
            SessionRegistry::new(),
            SwarmRegistry::with_limits(SwarmLimits {
                max_members: 0,
                max_active: 4,
            }),
            Arc::clone(&factory) as Arc<dyn WorkerFactory>,
        );

        let request = SpawnRequest::new(swarm(), SessionId::new(), "w", "do it");
        assert!(matches!(
            host.spawn(&request).await,
            Err(SpawnError::Admission(SwarmError::MemberCapReached { .. }))
        ));
        assert_eq!(
            factory.calls.load(Ordering::SeqCst),
            0,
            "a refused spawn must not pay for building a session"
        );
    }

    #[tokio::test]
    async fn a_retried_spawn_does_not_build_a_second_worker() {
        let factory = TestFactory::new(|_| Err("still failing".into()));
        let host = SwarmHost::new(
            SessionRegistry::new(),
            SwarmRegistry::new(),
            Arc::clone(&factory) as Arc<dyn WorkerFactory>,
        );
        let request = SpawnRequest::new(swarm(), SessionId::new(), "w", "do it").with_key("k1");

        // Both attempts fail in the factory and release their slot, so the key
        // never settles — which is correct: nothing was produced to be idempotent
        // about.
        assert!(host.spawn(&request).await.is_err());
        assert!(host.spawn(&request).await.is_err());
        assert_eq!(factory.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_report_message_names_the_worker_and_survives_an_empty_answer() {
        let session = SessionId::new();
        let message = report_message("reviewer", session, "found two problems", false);
        assert!(message.contains("reviewer"));
        assert!(message.contains(&session.to_string()));
        assert!(message.contains("found two problems"));

        let empty = report_message("", session, "   ", false);
        assert!(empty.contains("finished without reporting anything"));
        assert!(empty.contains(&session.to_string()));

        let cut = report_message("w", session, "a very long answer", true);
        assert!(cut.contains("[report truncated]"));
    }
}
