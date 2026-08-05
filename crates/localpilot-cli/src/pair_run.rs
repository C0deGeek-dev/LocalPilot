//! Terminal-free supervision for an interactive two-session collaboration.
//!
//! This module owns driver task lifetime, attributed runtime streams, progress,
//! and cooperative shutdown. The full-screen host remains the sole owner of the
//! actual terminal and maps these typed events onto presentation state.

use std::collections::VecDeque;

use localpilot_core::SessionId;
use localpilot_harness::RuntimeEvent;
use localpilot_server::swarm::{
    PairAbort, PairBounds, PairDriver, PairOutcome, PairProgress, PairProgressRx, PairReport,
    PairSetupError,
};
use tokio::sync::broadcast;
use tokio::task::{JoinError, JoinHandle};

use crate::interactive_session::{InteractivePairHost, InteractivePairOwner, PairPeer};

/// Detail-free terminal categories suitable for the basic run status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairTerminalStatus {
    Converged,
    CapReached,
    ProtocolError,
    Aborted,
    TimedOut,
    PeerFailed,
    ProviderError,
    BudgetExceeded,
    NoProgress,
    DriverFailed,
    Unknown,
}

/// Whether the driver is still running or has reached a terminal category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairRunState {
    Running,
    Finished(PairTerminalStatus),
}

/// The small progress projection needed before richer result presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PairRunStatus {
    pub(crate) state: PairRunState,
    pub(crate) completed_rounds: u32,
    pub(crate) max_rounds: u32,
    pub(crate) scheduled: Option<PairPeer>,
}

/// A single attributed update from the driver supervisor.
#[derive(Debug)]
pub(crate) enum PairPumpEvent {
    Runtime {
        peer: PairPeer,
        event: RuntimeEvent,
    },
    Progress(PairRunStatus),
    RuntimeLagged {
        peer: PairPeer,
        skipped: u64,
    },
    RuntimeClosed {
        peer: PairPeer,
    },
    InvariantViolation {
        detail: String,
    },
    Finished(PairRunStatus),
    DriverFailed {
        detail: String,
        status: PairRunStatus,
    },
}

/// A retained driver result after all hosted resources have been closed.
pub(crate) enum PairRunCompletion {
    Report(PairReport),
    DriverFailed(String),
}

impl PairRunCompletion {
    pub(crate) fn report(&self) -> Option<&PairReport> {
        match self {
            Self::Report(report) => Some(report),
            Self::DriverFailed(_) => None,
        }
    }

    pub(crate) fn driver_failure(&self) -> Option<&str> {
        match self {
            Self::Report(_) => None,
            Self::DriverFailed(detail) => Some(detail),
        }
    }

    pub(crate) fn terminal_status(&self) -> PairTerminalStatus {
        match self {
            Self::Report(report) => terminal_status(report.reason()),
            Self::DriverFailed(_) => PairTerminalStatus::DriverFailed,
        }
    }
}

/// Why a run could not cross its pre-spawn boundary.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PairRunSetupIssue {
    #[error(transparent)]
    Driver(#[from] PairSetupError),
    #[error("driver progress named unknown session {0}")]
    ForeignScheduledPeer(SessionId),
}

/// A setup error coupled to the intact host that still needs explicit cleanup.
pub(crate) struct PairRunSetupFailure {
    issue: PairRunSetupIssue,
    host: InteractivePairHost,
}

impl std::fmt::Debug for PairRunSetupFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PairRunSetupFailure")
            .field("issue", &self.issue)
            .finish_non_exhaustive()
    }
}

impl PairRunSetupFailure {
    pub(crate) fn issue(&self) -> &PairRunSetupIssue {
        &self.issue
    }

    pub(crate) fn into_parts(self) -> (PairRunSetupIssue, InteractivePairHost) {
        (self.issue, self.host)
    }
}

/// A validated driver and intact host that have not started model work yet.
#[must_use = "a prepared collaboration must be spawned or its host explicitly closed"]
pub(crate) struct PreparedPairRun {
    host: InteractivePairHost,
    driver: PairDriver,
    abort: PairAbort,
    progress: PairProgressRx,
    status: PairRunStatus,
}

impl PreparedPairRun {
    pub(crate) fn new(
        host: InteractivePairHost,
        bounds: PairBounds,
    ) -> Result<Self, PairRunSetupFailure> {
        let sessions = host.sessions();
        let driver = match PairDriver::new(sessions[0], sessions[1], host.task(), bounds) {
            Ok(driver) => driver,
            Err(error) => {
                return Err(PairRunSetupFailure {
                    issue: error.into(),
                    host,
                })
            }
        };
        let abort = driver.abort_handle();
        let progress = driver.progress();
        let status = match status_from_progress(&progress.latest(), sessions) {
            Ok(status) => status,
            Err(issue) => return Err(PairRunSetupFailure { issue, host }),
        };
        Ok(Self {
            host,
            driver,
            abort,
            progress,
            status,
        })
    }

    /// Recover the intact host when terminal setup or another pre-spawn step
    /// fails. No driver task or model turn has started yet.
    pub(crate) fn into_host(self) -> InteractivePairHost {
        self.host
    }

    /// Start the sole task allowed to drive either peer.
    pub(crate) fn spawn(self) -> InteractivePairRun {
        let Self {
            host,
            driver,
            abort,
            progress,
            status,
        } = self;
        let (mut adopted, owner) = host.into_parts();
        let driver = tokio::spawn(async move { driver.run(&mut adopted).await });
        InteractivePairRun {
            owner,
            abort,
            progress,
            progress_open: true,
            driver: Some(driver),
            completion: None,
            status,
            runtime_open: [true, true],
            drain_next: PairPeer::A,
            pending: VecDeque::new(),
            terminal_emitted: false,
        }
    }
}

/// A live or completed collaboration whose hosted resources remain UI-owned.
#[must_use = "a live collaboration must be shut down cooperatively"]
pub(crate) struct InteractivePairRun {
    owner: InteractivePairOwner,
    abort: PairAbort,
    progress: PairProgressRx,
    progress_open: bool,
    driver: Option<JoinHandle<PairReport>>,
    completion: Option<PairRunCompletion>,
    status: PairRunStatus,
    runtime_open: [bool; 2],
    drain_next: PairPeer,
    pending: VecDeque<PairPumpEvent>,
    terminal_emitted: bool,
}

enum PumpReady {
    Runtime(PairPeer, Result<RuntimeEvent, broadcast::error::RecvError>),
    Progress(Option<PairProgress>),
    Driver(Result<PairReport, JoinError>),
}

impl InteractivePairRun {
    pub(crate) fn status(&self) -> PairRunStatus {
        self.status
    }

    pub(crate) fn completion(&self) -> Option<&PairRunCompletion> {
        self.completion.as_ref()
    }

    pub(crate) fn is_driver_live(&self) -> bool {
        self.driver.is_some()
    }

    /// Abort the protocol and reach both exact in-flight session hosts.
    pub(crate) fn abort_and_cancel(&self) {
        self.abort.abort();
        self.owner.a.host.cancel();
        self.owner.b.host.cancel();
    }

    /// Await the next attributed update. A completed pump returns `None` after
    /// every buffered runtime event and its one terminal notification.
    pub(crate) async fn next(&mut self) -> Option<PairPumpEvent> {
        loop {
            if self.completion.is_some() {
                if let Some(event) = self.drain_one() {
                    return Some(event);
                }
                if let Some(event) = self.pending.pop_front() {
                    return Some(event);
                }
                if self.terminal_emitted {
                    return None;
                }
                self.terminal_emitted = true;
                return Some(self.terminal_event());
            }

            let ready = {
                let Some(driver) = self.driver.as_mut() else {
                    self.status.state = PairRunState::Finished(PairTerminalStatus::DriverFailed);
                    self.status.scheduled = None;
                    self.completion = Some(PairRunCompletion::DriverFailed(
                        "driver state disappeared before completion".to_string(),
                    ));
                    continue;
                };
                tokio::select! {
                    event = self.owner.a.events.recv(), if self.runtime_open[0] => {
                        PumpReady::Runtime(PairPeer::A, event)
                    }
                    event = self.owner.b.events.recv(), if self.runtime_open[1] => {
                        PumpReady::Runtime(PairPeer::B, event)
                    }
                    progress = self.progress.changed(), if self.progress_open => {
                        PumpReady::Progress(progress)
                    }
                    result = driver => PumpReady::Driver(result),
                }
            };

            match ready {
                PumpReady::Runtime(peer, Ok(event)) => {
                    return Some(PairPumpEvent::Runtime { peer, event })
                }
                PumpReady::Runtime(peer, Err(broadcast::error::RecvError::Lagged(skipped))) => {
                    return Some(PairPumpEvent::RuntimeLagged { peer, skipped })
                }
                PumpReady::Runtime(peer, Err(broadcast::error::RecvError::Closed)) => {
                    self.runtime_open[peer_index(peer)] = false;
                    self.abort_and_cancel();
                    return Some(PairPumpEvent::RuntimeClosed { peer });
                }
                PumpReady::Progress(Some(progress)) => {
                    match status_from_progress(&progress, self.owner.sessions) {
                        Ok(status) => {
                            self.status = status;
                            return Some(PairPumpEvent::Progress(status));
                        }
                        Err(issue) => {
                            // Fuse the invalid source so it cannot repeat while
                            // cooperative abort brings the driver to completion.
                            self.progress_open = false;
                            self.abort_and_cancel();
                            return Some(PairPumpEvent::InvariantViolation {
                                detail: issue.to_string(),
                            });
                        }
                    }
                }
                PumpReady::Progress(None) => {
                    // A closed watch stays ready forever; fuse it and let the
                    // driver join carry the authoritative terminal result.
                    self.progress_open = false;
                }
                PumpReady::Driver(result) => {
                    self.driver.take();
                    self.record_driver_result(result);
                }
            }
        }
    }

    /// Complete shutdown after the terminal owner has restored terminal modes.
    ///
    /// A live driver is signalled and awaited cooperatively; it is never aborted,
    /// detached, or dropped. Hosted ownership is then closed in lifecycle order.
    pub(crate) async fn shutdown(mut self) -> PairRunCompletion {
        if let Some(driver) = self.driver.take() {
            self.abort_and_cancel();
            self.record_driver_result(driver.await);
        }
        let completion = self.completion.take().unwrap_or_else(|| {
            PairRunCompletion::DriverFailed(
                "driver ended without a retained completion".to_string(),
            )
        });
        self.owner.close().await;
        completion
    }

    fn record_driver_result(&mut self, result: Result<PairReport, JoinError>) {
        let (terminal, completion) = match result {
            Ok(report) => (
                terminal_status(report.reason()),
                PairRunCompletion::Report(report),
            ),
            Err(error) => {
                self.abort_and_cancel();
                let detail = error.to_string();
                (
                    PairTerminalStatus::DriverFailed,
                    PairRunCompletion::DriverFailed(detail),
                )
            }
        };

        match status_from_progress(&self.progress.latest(), self.owner.sessions) {
            Ok(mut status) => {
                status.state = PairRunState::Finished(terminal);
                status.scheduled = None;
                self.status = status;
            }
            Err(issue) => {
                self.status.state = PairRunState::Finished(terminal);
                self.status.scheduled = None;
                self.pending.push_back(PairPumpEvent::InvariantViolation {
                    detail: issue.to_string(),
                });
            }
        }
        self.progress_open = false;
        self.completion = Some(completion);
    }

    /// Drain at most one buffered event, alternating the first peer examined on
    /// successive calls. The driver is already done, so `Empty` is final for the
    /// current buffer and a closed sender is not a run failure.
    fn drain_one(&mut self) -> Option<PairPumpEvent> {
        for _ in 0..2 {
            let peer = self.drain_next;
            self.drain_next = other_peer(peer);
            let index = peer_index(peer);
            if !self.runtime_open[index] {
                continue;
            }
            let received = match peer {
                PairPeer::A => self.owner.a.events.try_recv(),
                PairPeer::B => self.owner.b.events.try_recv(),
            };
            match received {
                Ok(event) => return Some(PairPumpEvent::Runtime { peer, event }),
                Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                    return Some(PairPumpEvent::RuntimeLagged { peer, skipped })
                }
                Err(broadcast::error::TryRecvError::Empty) => {}
                Err(broadcast::error::TryRecvError::Closed) => {
                    self.runtime_open[index] = false;
                }
            }
        }
        None
    }

    fn terminal_event(&self) -> PairPumpEvent {
        match self.completion.as_ref() {
            Some(PairRunCompletion::Report(_)) => PairPumpEvent::Finished(self.status),
            Some(PairRunCompletion::DriverFailed(detail)) => PairPumpEvent::DriverFailed {
                detail: detail.clone(),
                status: self.status,
            },
            None => PairPumpEvent::DriverFailed {
                detail: "driver ended without a retained completion".to_string(),
                status: self.status,
            },
        }
    }
}

fn status_from_progress(
    progress: &PairProgress,
    sessions: [SessionId; 2],
) -> Result<PairRunStatus, PairRunSetupIssue> {
    let scheduled = match progress.next_peer() {
        Some(session) if session == sessions[0] => Some(PairPeer::A),
        Some(session) if session == sessions[1] => Some(PairPeer::B),
        Some(session) => return Err(PairRunSetupIssue::ForeignScheduledPeer(session)),
        None => None,
    };
    Ok(PairRunStatus {
        state: PairRunState::Running,
        completed_rounds: progress.completed_rounds(),
        max_rounds: progress.max_rounds(),
        scheduled,
    })
}

fn terminal_status(outcome: &PairOutcome) -> PairTerminalStatus {
    match outcome {
        PairOutcome::Converged { .. } => PairTerminalStatus::Converged,
        PairOutcome::CapReached { .. } => PairTerminalStatus::CapReached,
        PairOutcome::ProtocolError { .. } => PairTerminalStatus::ProtocolError,
        PairOutcome::Aborted => PairTerminalStatus::Aborted,
        PairOutcome::TimedOut => PairTerminalStatus::TimedOut,
        PairOutcome::PeerFailed { .. } => PairTerminalStatus::PeerFailed,
        PairOutcome::ProviderError { .. } => PairTerminalStatus::ProviderError,
        PairOutcome::BudgetExceeded => PairTerminalStatus::BudgetExceeded,
        PairOutcome::NoProgress => PairTerminalStatus::NoProgress,
        _ => PairTerminalStatus::Unknown,
    }
}

const fn peer_index(peer: PairPeer) -> usize {
    match peer {
        PairPeer::A => 0,
        PairPeer::B => 1,
    }
}

const fn other_peer(peer: PairPeer) -> PairPeer {
    match peer {
        PairPeer::A => PairPeer::B,
        PairPeer::B => PairPeer::A,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use futures::StreamExt as _;
    use localpilot_config::{Config, ProviderConfig};
    use localpilot_llm::{
        FakeProvider, ModelEvent, ModelEventStream, ModelProvider, ModelRequest,
        ProviderDeclaration, ProviderError, ProviderRegistry,
    };
    use localpilot_sandbox::Profile;
    use localpilot_store::{SessionEventKind, Store};
    use tokio::sync::{mpsc, Notify};

    use super::*;
    use crate::interactive_session::{InteractivePeerSelection, InteractiveSessionSetup};

    const A_PROPOSAL: &str = r#"{"v":1,"action":"propose","artifact":"alpha"}"#;
    const B_PROPOSAL: &str = r#"{"v":1,"action":"propose","artifact":"beta"}"#;

    fn declaration(id: &str) -> ProviderDeclaration {
        let seed = FakeProvider::new();
        let mut declaration = seed.declaration().clone();
        declaration.id = id.to_string();
        declaration.display_name = id.to_string();
        declaration
    }

    fn fake(id: &str, response: &str) -> Arc<FakeProvider> {
        Arc::new(
            FakeProvider::new()
                .with_declaration(declaration(id))
                .text(response),
        )
    }

    fn setup(
        root: &std::path::Path,
        first: Arc<dyn ModelProvider>,
        second: Arc<dyn ModelProvider>,
    ) -> InteractiveSessionSetup {
        let providers =
            HashMap::from([("first".to_string(), first), ("second".to_string(), second)]);
        let models = HashMap::from([
            ("first".to_string(), "model-a".to_string()),
            ("second".to_string(), "model-b".to_string()),
        ]);
        let mut config = Config::default();
        config.provider.default = "first".to_string();
        config
            .providers
            .insert("first".to_string(), ProviderConfig::default());
        config
            .providers
            .insert("second".to_string(), ProviderConfig::default());
        InteractiveSessionSetup::for_test(
            root.to_path_buf(),
            config,
            Profile::Default,
            ProviderRegistry::from_providers(providers, models, "first"),
        )
    }

    async fn prepare(setup: &InteractiveSessionSetup) -> InteractivePairHost {
        InteractivePairHost::prepare(
            setup,
            "compare both proposals",
            InteractivePeerSelection {
                provider_id: "first",
                model: "model-a",
            },
            InteractivePeerSelection {
                provider_id: "second",
                model: "model-b",
            },
        )
        .await
        .expect("interactive pair")
    }

    fn bounds(max_rounds: u32) -> PairBounds {
        PairBounds {
            max_rounds,
            slot_timeout: Duration::from_secs(5),
            slot_token_budget: 0,
        }
    }

    struct PendingProvider {
        declaration: ProviderDeclaration,
    }

    impl PendingProvider {
        fn arc(id: &str) -> Arc<dyn ModelProvider> {
            Arc::new(Self {
                declaration: declaration(id),
            })
        }
    }

    #[async_trait]
    impl ModelProvider for PendingProvider {
        fn declaration(&self) -> &ProviderDeclaration {
            &self.declaration
        }

        async fn stream(&self, _request: ModelRequest) -> Result<ModelEventStream, ProviderError> {
            Ok(Box::pin(futures::stream::pending::<
                Result<ModelEvent, ProviderError>,
            >()))
        }
    }

    struct GatedProvider {
        declaration: ProviderDeclaration,
        label: &'static str,
        response: String,
        started: mpsc::UnboundedSender<&'static str>,
        release: Arc<Notify>,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ModelProvider for GatedProvider {
        fn declaration(&self) -> &ProviderDeclaration {
            &self.declaration
        }

        async fn stream(&self, _request: ModelRequest) -> Result<ModelEventStream, ProviderError> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            let _sent = self.started.send(self.label);
            self.release.notified().await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(futures::stream::iter(vec![
                Ok(ModelEvent::TextDelta(self.response.clone())),
                Ok(ModelEvent::Done),
            ])
            .boxed())
        }
    }

    #[tokio::test]
    async fn setup_failure_returns_the_intact_host_for_explicit_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let first = fake("first", A_PROPOSAL);
        let second = fake("second", B_PROPOSAL);
        let first_dyn: Arc<dyn ModelProvider> = first.clone();
        let second_dyn: Arc<dyn ModelProvider> = second.clone();
        let setup = setup(directory.path(), first_dyn, second_dyn);
        let host = prepare(&setup).await;
        let sessions = host.sessions();

        let failure = match PreparedPairRun::new(host, bounds(0)) {
            Ok(_) => panic!("zero rounds must fail before spawn"),
            Err(failure) => failure,
        };
        assert!(matches!(
            failure.issue(),
            PairRunSetupIssue::Driver(PairSetupError::ZeroRoundCap)
        ));
        let (issue, host) = failure.into_parts();
        assert!(matches!(
            issue,
            PairRunSetupIssue::Driver(PairSetupError::ZeroRoundCap)
        ));
        host.close().await;

        for session in sessions {
            assert_session_closed(directory.path(), session);
        }
        assert!(first.requests().is_empty());
        assert!(second.requests().is_empty());
    }

    #[tokio::test]
    async fn prepared_run_can_return_its_host_before_any_model_work_starts() {
        let directory = tempfile::tempdir().unwrap();
        let first = fake("first", A_PROPOSAL);
        let second = fake("second", B_PROPOSAL);
        let first_dyn: Arc<dyn ModelProvider> = first.clone();
        let second_dyn: Arc<dyn ModelProvider> = second.clone();
        let setup = setup(directory.path(), first_dyn, second_dyn);
        let prepared = PreparedPairRun::new(prepare(&setup).await, bounds(1)).unwrap();
        let sessions = prepared.host.sessions();

        prepared.into_host().close().await;

        for session in sessions {
            assert_session_closed(directory.path(), session);
        }
        assert!(first.requests().is_empty());
        assert!(second.requests().is_empty());
    }

    #[tokio::test]
    async fn pump_tags_streams_reports_lag_and_aborts_on_early_closure() {
        let directory = tempfile::tempdir().unwrap();
        let second = fake("second", B_PROPOSAL);
        let second_dyn: Arc<dyn ModelProvider> = second.clone();
        let setup = setup(directory.path(), PendingProvider::arc("first"), second_dyn);
        let mut run = PreparedPairRun::new(prepare(&setup).await, bounds(2))
            .unwrap()
            .spawn();
        let (a_tx, a_rx) = broadcast::channel(1);
        let (b_tx, b_rx) = broadcast::channel(2);
        run.owner.a.events = a_rx;
        run.owner.b.events = b_rx;

        a_tx.send(RuntimeEvent::Text("discarded".to_string()))
            .unwrap();
        a_tx.send(RuntimeEvent::Text("alpha".to_string())).unwrap();
        b_tx.send(RuntimeEvent::Warning("beta".to_string()))
            .unwrap();

        let mut saw_a_lag = false;
        let mut saw_a = false;
        let mut saw_b = false;
        for _ in 0..3 {
            match run.next().await.unwrap() {
                PairPumpEvent::RuntimeLagged {
                    peer: PairPeer::A,
                    skipped: 1,
                } => saw_a_lag = true,
                PairPumpEvent::Runtime {
                    peer: PairPeer::B,
                    event: RuntimeEvent::Warning(message),
                } if message == "beta" => saw_b = true,
                PairPumpEvent::Runtime {
                    peer: PairPeer::A,
                    event: RuntimeEvent::Text(message),
                } if message == "alpha" => saw_a = true,
                other => panic!("unexpected routed event: {other:?}"),
            }
        }
        assert!(saw_a_lag && saw_a && saw_b);

        drop(b_tx);
        if !matches!(
            run.next().await.unwrap(),
            PairPumpEvent::RuntimeClosed { peer: PairPeer::B }
        ) {
            panic!("peer B closure was not attributed");
        }

        let terminal = loop {
            match run.next().await.unwrap() {
                PairPumpEvent::Progress(_) => {}
                PairPumpEvent::Finished(status) => break status,
                other => panic!("unexpected shutdown event: {other:?}"),
            }
        };
        assert_eq!(
            terminal.state,
            PairRunState::Finished(PairTerminalStatus::Aborted)
        );
        assert!(matches!(
            run.completion()
                .and_then(PairRunCompletion::report)
                .map(PairReport::reason),
            Some(PairOutcome::Aborted)
        ));
        let completion = run.shutdown().await;
        assert_eq!(completion.terminal_status(), PairTerminalStatus::Aborted);
        assert!(second.requests().is_empty());
        drop(a_tx);
    }

    #[tokio::test]
    async fn buffered_runtime_events_precede_one_terminal_notification() {
        let directory = tempfile::tempdir().unwrap();
        let first = fake("first", A_PROPOSAL);
        let second = fake("second", B_PROPOSAL);
        let first_dyn: Arc<dyn ModelProvider> = first.clone();
        let second_dyn: Arc<dyn ModelProvider> = second.clone();
        let setup = setup(directory.path(), first_dyn, second_dyn);
        let mut run = PreparedPairRun::new(prepare(&setup).await, bounds(1))
            .unwrap()
            .spawn();
        let (a_tx, a_rx) = broadcast::channel(8);
        let (b_tx, b_rx) = broadcast::channel(8);
        run.owner.a.events = a_rx;
        run.owner.b.events = b_rx;
        a_tx.send(RuntimeEvent::Text("last-a".to_string())).unwrap();
        b_tx.send(RuntimeEvent::Text("last-b".to_string())).unwrap();

        let driver = run.driver.take().unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), driver)
            .await
            .expect("driver finishes");
        run.record_driver_result(result);

        let mut routed = Vec::new();
        let terminal = loop {
            match run.next().await.unwrap() {
                PairPumpEvent::Runtime {
                    peer,
                    event: RuntimeEvent::Text(message),
                } => routed.push((peer, message)),
                PairPumpEvent::Progress(_) => {}
                PairPumpEvent::Finished(status) => break status,
                other => panic!("unexpected final-drain event: {other:?}"),
            }
        };
        assert_eq!(
            routed,
            vec![
                (PairPeer::A, "last-a".to_string()),
                (PairPeer::B, "last-b".to_string())
            ]
        );
        assert_eq!(terminal.completed_rounds, 1);
        assert_eq!(terminal.max_rounds, 1);
        assert_eq!(terminal.scheduled, None);
        assert_eq!(
            terminal.state,
            PairRunState::Finished(PairTerminalStatus::CapReached)
        );
        assert!(
            run.next().await.is_none(),
            "terminal notification is one-shot"
        );
        assert!(run.next().await.is_none(), "closed progress is fused");
        let completion = run.shutdown().await;
        assert_eq!(completion.terminal_status(), PairTerminalStatus::CapReached);
        assert_eq!(first.requests().len(), 1);
        assert_eq!(second.requests().len(), 1);
        drop((a_tx, b_tx));
    }

    #[tokio::test]
    async fn join_failure_is_retained_and_emitted_once() {
        let directory = tempfile::tempdir().unwrap();
        let setup = setup(
            directory.path(),
            PendingProvider::arc("first"),
            PendingProvider::arc("second"),
        );
        let mut run = PreparedPairRun::new(prepare(&setup).await, bounds(2))
            .unwrap()
            .spawn();
        run.abort_and_cancel();
        let original = run.driver.take().unwrap();
        let report = original.await.unwrap();
        assert_eq!(report.reason(), &PairOutcome::Aborted);
        run.progress_open = false;
        run.driver = Some(tokio::spawn(async move {
            panic!("driver task panic fixture");
        }));

        let (detail, status) = match run.next().await.unwrap() {
            PairPumpEvent::DriverFailed { detail, status } => (detail, status),
            other => panic!("expected driver failure, got {other:?}"),
        };
        assert!(detail.contains("panicked"));
        assert_eq!(
            status.state,
            PairRunState::Finished(PairTerminalStatus::DriverFailed)
        );
        assert!(run.next().await.is_none());
        assert!(run
            .completion()
            .and_then(PairRunCompletion::driver_failure)
            .is_some_and(|message| message.contains("panicked")));
        let completion = run.shutdown().await;
        assert_eq!(
            completion.terminal_status(),
            PairTerminalStatus::DriverFailed
        );
    }

    #[tokio::test]
    async fn shutdown_cancels_awaits_and_closes_a_pending_real_turn() {
        let directory = tempfile::tempdir().unwrap();
        let second = fake("second", B_PROPOSAL);
        let second_dyn: Arc<dyn ModelProvider> = second.clone();
        let setup = setup(directory.path(), PendingProvider::arc("first"), second_dyn);
        let run = PreparedPairRun::new(prepare(&setup).await, bounds(2))
            .unwrap()
            .spawn();
        let sessions = run.owner.sessions;
        let registry = run.owner.registry.clone();
        let swarm_host = run.owner.swarm_host.clone();
        let a_host = Arc::clone(&run.owner.a.host);

        tokio::time::timeout(Duration::from_secs(5), async {
            while !a_host.is_busy() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("A enters its turn");
        let completion = run.shutdown().await;

        assert_eq!(completion.terminal_status(), PairTerminalStatus::Aborted);
        assert!(!a_host.is_busy());
        assert!(registry.is_empty().await);
        for session in sessions {
            assert!(swarm_host.host(session).await.is_none());
            assert_session_closed(directory.path(), session);
        }
        assert!(second.requests().is_empty());
    }

    #[tokio::test]
    async fn the_cli_supervisor_never_overlaps_peer_model_turns() {
        let directory = tempfile::tempdir().unwrap();
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let release_a = Arc::new(Notify::new());
        let release_b = Arc::new(Notify::new());
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let a: Arc<dyn ModelProvider> = Arc::new(GatedProvider {
            declaration: declaration("first"),
            label: "A",
            response: A_PROPOSAL.to_string(),
            started: started_tx.clone(),
            release: Arc::clone(&release_a),
            active: Arc::clone(&active),
            max_active: Arc::clone(&max_active),
        });
        let b: Arc<dyn ModelProvider> = Arc::new(GatedProvider {
            declaration: declaration("second"),
            label: "B",
            response: B_PROPOSAL.to_string(),
            started: started_tx,
            release: Arc::clone(&release_b),
            active: Arc::clone(&active),
            max_active: Arc::clone(&max_active),
        });
        let setup = setup(directory.path(), a, b);
        let mut run = PreparedPairRun::new(prepare(&setup).await, bounds(1))
            .unwrap()
            .spawn();
        assert_eq!(run.status().scheduled, Some(PairPeer::A));

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), started_rx.recv())
                .await
                .unwrap(),
            Some("A")
        );
        assert!(run.owner.a.host.is_busy());
        assert!(!run.owner.b.host.is_busy());
        release_a.notify_one();

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), started_rx.recv())
                .await
                .unwrap(),
            Some("B")
        );
        assert!(!run.owner.a.host.is_busy());
        assert!(run.owner.b.host.is_busy());
        let scheduled = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match run.next().await.unwrap() {
                    PairPumpEvent::Runtime { .. } => {}
                    PairPumpEvent::Progress(status) if status.scheduled == Some(PairPeer::B) => {
                        break status
                    }
                    PairPumpEvent::Progress(_) => {}
                    other => panic!("unexpected in-flight progress event: {other:?}"),
                }
            }
        })
        .await
        .expect("B progress is observable");
        assert_eq!(scheduled.completed_rounds, 0);
        assert_eq!(scheduled.state, PairRunState::Running);
        release_b.notify_one();

        loop {
            match tokio::time::timeout(Duration::from_secs(5), run.next())
                .await
                .expect("pump remains responsive")
                .expect("terminal event")
            {
                PairPumpEvent::Runtime { .. } | PairPumpEvent::Progress(_) => {}
                PairPumpEvent::Finished(status) => {
                    assert_eq!(
                        status.state,
                        PairRunState::Finished(PairTerminalStatus::CapReached)
                    );
                    break;
                }
                other => panic!("unexpected no-overlap event: {other:?}"),
            }
        }
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(max_active.load(Ordering::SeqCst), 1);
        let completion = run.shutdown().await;
        assert_eq!(completion.terminal_status(), PairTerminalStatus::CapReached);
    }

    fn assert_session_closed(root: &std::path::Path, session: SessionId) {
        let events = Store::open(root).read_events(session).unwrap();
        assert!(events
            .iter()
            .any(|event| event.kind == SessionEventKind::SessionClosed));
    }
}
