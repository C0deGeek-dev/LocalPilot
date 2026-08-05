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
use localpilot_tools::{UserAnswer, UserQuestion};
use localpilot_tui::ApprovalRequest;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::{JoinError, JoinHandle};

use crate::interactive_session::{
    ApprovalCall, InteractivePairHost, InteractivePairOwner, PairPeer, QuestionCall,
};

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

/// Stable identity for one user decision requested by a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PairAskId(u64);

/// The channel family that produced a user decision request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairAskKind {
    Approval,
    Questions,
}

/// A terminal-neutral request that can be attributed before it is rendered.
#[derive(Debug, Clone)]
pub(crate) enum PairAskRequest {
    Approval(ApprovalRequest),
    Questions(Vec<UserQuestion>),
}

/// The visible clone of a request whose exact reply channel stays private.
#[derive(Debug, Clone)]
pub(crate) struct PairAsk {
    pub(crate) id: PairAskId,
    pub(crate) peer: PairPeer,
    pub(crate) request: PairAskRequest,
}

/// A typed response to the currently active request.
#[derive(Debug)]
pub(crate) enum PairAskAnswer {
    Approval(bool),
    Questions(Vec<UserAnswer>),
}

impl PairAskAnswer {
    const fn kind(&self) -> PairAskKind {
        match self {
            Self::Approval(_) => PairAskKind::Approval,
            Self::Questions(_) => PairAskKind::Questions,
        }
    }
}

/// Why an answer could not be delivered to the active requester.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum PairAskAnswerError {
    #[error("there is no active request for answer {received:?}")]
    NoActive { received: PairAskId },
    #[error("answer {received:?} does not match active request {active:?}")]
    Stale {
        received: PairAskId,
        active: PairAskId,
    },
    #[error("request {id:?} expects {expected:?}, not {received:?}")]
    WrongKind {
        id: PairAskId,
        expected: PairAskKind,
        received: PairAskKind,
    },
    #[error("request {id:?} expects {expected} question answers, not {received}")]
    WrongQuestionCount {
        id: PairAskId,
        expected: usize,
        received: usize,
    },
    #[error("the requester for {id:?} ended before its answer arrived")]
    RequesterGone { id: PairAskId },
}

/// A single attributed update from the driver supervisor.
#[derive(Debug)]
pub(crate) enum PairPumpEvent {
    Runtime {
        peer: PairPeer,
        event: RuntimeEvent,
    },
    Ask(PairAsk),
    Progress(PairRunStatus),
    RuntimeLagged {
        peer: PairPeer,
        skipped: u64,
    },
    RuntimeClosed {
        peer: PairPeer,
    },
    AskChannelClosed {
        peer: PairPeer,
        kind: PairAskKind,
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

enum PendingPairAsk {
    Approval {
        id: PairAskId,
        peer: PairPeer,
        request: ApprovalRequest,
        reply: oneshot::Sender<bool>,
    },
    Questions {
        id: PairAskId,
        peer: PairPeer,
        questions: Vec<UserQuestion>,
        reply: oneshot::Sender<Vec<UserAnswer>>,
    },
}

impl PendingPairAsk {
    const fn id(&self) -> PairAskId {
        match self {
            Self::Approval { id, .. } | Self::Questions { id, .. } => *id,
        }
    }

    const fn kind(&self) -> PairAskKind {
        match self {
            Self::Approval { .. } => PairAskKind::Approval,
            Self::Questions { .. } => PairAskKind::Questions,
        }
    }

    fn view(&self) -> PairAsk {
        match self {
            Self::Approval {
                id, peer, request, ..
            } => PairAsk {
                id: *id,
                peer: *peer,
                request: PairAskRequest::Approval(request.clone()),
            },
            Self::Questions {
                id,
                peer,
                questions,
                ..
            } => PairAsk {
                id: *id,
                peer: *peer,
                request: PairAskRequest::Questions(questions.clone()),
            },
        }
    }

    fn fail_closed(self) {
        match self {
            Self::Approval { reply, .. } => {
                let _ = reply.send(false);
            }
            Self::Questions {
                questions, reply, ..
            } => {
                let _ = reply.send(vec![UserAnswer::Dismissed; questions.len()]);
            }
        }
    }

    fn answer(self, answer: PairAskAnswer) -> bool {
        match (self, answer) {
            (Self::Approval { reply, .. }, PairAskAnswer::Approval(approved)) => {
                reply.send(approved).is_ok()
            }
            (Self::Questions { reply, .. }, PairAskAnswer::Questions(answers)) => {
                reply.send(answers).is_ok()
            }
            _ => false,
        }
    }
}

enum ObservedPairAsk {
    Approval { peer: PairPeer, call: ApprovalCall },
    Questions { peer: PairPeer, call: QuestionCall },
}

impl ObservedPairAsk {
    fn fail_closed(self) {
        match self {
            Self::Approval { call, .. } => deny_approval(call),
            Self::Questions { call, .. } => dismiss_questions(call),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum PairAskSource {
    AApproval,
    AQuestions,
    BApproval,
    BQuestions,
}

impl PairAskSource {
    const ALL: [Self; 4] = [
        Self::AApproval,
        Self::AQuestions,
        Self::BApproval,
        Self::BQuestions,
    ];

    const fn peer(self) -> PairPeer {
        match self {
            Self::AApproval | Self::AQuestions => PairPeer::A,
            Self::BApproval | Self::BQuestions => PairPeer::B,
        }
    }

    const fn kind(self) -> PairAskKind {
        match self {
            Self::AApproval | Self::BApproval => PairAskKind::Approval,
            Self::AQuestions | Self::BQuestions => PairAskKind::Questions,
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::AApproval => 0,
            Self::AQuestions => 1,
            Self::BApproval => 2,
            Self::BQuestions => 3,
        }
    }
}

#[derive(Debug)]
enum PairAskObservationError {
    ChannelClosed(PairAskSource),
    IdExhausted,
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
            ask_open: [true; 4],
            next_ask_id: Some(1),
            active_ask: None,
            queued_asks: VecDeque::new(),
            active_needs_emit: false,
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
    ask_open: [bool; 4],
    next_ask_id: Option<u64>,
    active_ask: Option<PendingPairAsk>,
    queued_asks: VecDeque<PendingPairAsk>,
    active_needs_emit: bool,
    drain_next: PairPeer,
    pending: VecDeque<PairPumpEvent>,
    terminal_emitted: bool,
}

enum PumpReady {
    Runtime(PairPeer, Result<RuntimeEvent, broadcast::error::RecvError>),
    Approval(PairPeer, Option<ApprovalCall>),
    Questions(PairPeer, Option<QuestionCall>),
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

    /// Fail closed every user request, then reach the driver and both hosts.
    pub(crate) fn abort_and_cancel(&mut self) {
        self.fail_closed_asks();
        self.abort_driver_and_hosts();
    }

    /// Answer only the active request whose identity the pump emitted.
    pub(crate) fn answer_ask(
        &mut self,
        id: PairAskId,
        answer: PairAskAnswer,
    ) -> Result<(), PairAskAnswerError> {
        let Some(active) = self.active_ask.as_ref() else {
            return Err(PairAskAnswerError::NoActive { received: id });
        };
        if active.id() != id {
            return Err(PairAskAnswerError::Stale {
                received: id,
                active: active.id(),
            });
        }

        let expected_kind = active.kind();
        let received_kind = answer.kind();
        if expected_kind != received_kind {
            return Err(PairAskAnswerError::WrongKind {
                id,
                expected: expected_kind,
                received: received_kind,
            });
        }
        if let (PendingPairAsk::Questions { questions, .. }, PairAskAnswer::Questions(answers)) =
            (active, &answer)
        {
            if questions.len() != answers.len() {
                return Err(PairAskAnswerError::WrongQuestionCount {
                    id,
                    expected: questions.len(),
                    received: answers.len(),
                });
            }
        }

        let Some(active) = self.active_ask.take() else {
            return Err(PairAskAnswerError::NoActive { received: id });
        };
        self.active_needs_emit = false;
        let delivered = active.answer(answer);
        self.promote_queued_ask();
        if delivered {
            Ok(())
        } else {
            Err(PairAskAnswerError::RequesterGone { id })
        }
    }

    fn abort_driver_and_hosts(&self) {
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

            // Preserve a deterministic tie-break for calls already buffered,
            // then surface a waiting human decision before hot runtime streams.
            if let Err(error) = self.scan_buffered_asks() {
                return Some(self.stop_for_ask_error(error));
            }
            if self.active_needs_emit {
                self.active_needs_emit = false;
                if let Some(active) = self.active_ask.as_ref() {
                    return Some(PairPumpEvent::Ask(active.view()));
                }
            }

            let ready = {
                let Some(driver) = self.driver.as_mut() else {
                    self.fail_closed_asks();
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
                    call = self.owner.a.approvals.recv(), if self.ask_open[0] => {
                        PumpReady::Approval(PairPeer::A, call)
                    }
                    call = self.owner.a.questions.recv(), if self.ask_open[1] => {
                        PumpReady::Questions(PairPeer::A, call)
                    }
                    call = self.owner.b.approvals.recv(), if self.ask_open[2] => {
                        PumpReady::Approval(PairPeer::B, call)
                    }
                    call = self.owner.b.questions.recv(), if self.ask_open[3] => {
                        PumpReady::Questions(PairPeer::B, call)
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
                PumpReady::Approval(peer, Some(call)) => {
                    if let Err(error) = self.observe_ask(ObservedPairAsk::Approval { peer, call }) {
                        return Some(self.stop_for_ask_error(error));
                    }
                }
                PumpReady::Questions(peer, Some(call)) => {
                    if let Err(error) = self.observe_ask(ObservedPairAsk::Questions { peer, call })
                    {
                        return Some(self.stop_for_ask_error(error));
                    }
                }
                PumpReady::Approval(peer, None) => {
                    return Some(
                        self.stop_for_ask_error(PairAskObservationError::ChannelClosed(
                            match peer {
                                PairPeer::A => PairAskSource::AApproval,
                                PairPeer::B => PairAskSource::BApproval,
                            },
                        )),
                    );
                }
                PumpReady::Questions(peer, None) => {
                    return Some(
                        self.stop_for_ask_error(PairAskObservationError::ChannelClosed(
                            match peer {
                                PairPeer::A => PairAskSource::AQuestions,
                                PairPeer::B => PairAskSource::BQuestions,
                            },
                        )),
                    );
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

    fn scan_buffered_asks(&mut self) -> Result<(), PairAskObservationError> {
        for source in PairAskSource::ALL {
            if !self.ask_open[source.index()] {
                continue;
            }
            let observed = match self.try_receive_ask(source) {
                Ok(observed) => observed,
                Err(mpsc::error::TryRecvError::Empty) => continue,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    return Err(PairAskObservationError::ChannelClosed(source))
                }
            };
            self.observe_ask(observed)?;
        }
        Ok(())
    }

    fn try_receive_ask(
        &mut self,
        source: PairAskSource,
    ) -> Result<ObservedPairAsk, mpsc::error::TryRecvError> {
        match source {
            PairAskSource::AApproval => {
                self.owner
                    .a
                    .approvals
                    .try_recv()
                    .map(|call| ObservedPairAsk::Approval {
                        peer: PairPeer::A,
                        call,
                    })
            }
            PairAskSource::AQuestions => {
                self.owner
                    .a
                    .questions
                    .try_recv()
                    .map(|call| ObservedPairAsk::Questions {
                        peer: PairPeer::A,
                        call,
                    })
            }
            PairAskSource::BApproval => {
                self.owner
                    .b
                    .approvals
                    .try_recv()
                    .map(|call| ObservedPairAsk::Approval {
                        peer: PairPeer::B,
                        call,
                    })
            }
            PairAskSource::BQuestions => {
                self.owner
                    .b
                    .questions
                    .try_recv()
                    .map(|call| ObservedPairAsk::Questions {
                        peer: PairPeer::B,
                        call,
                    })
            }
        }
    }

    fn observe_ask(&mut self, observed: ObservedPairAsk) -> Result<(), PairAskObservationError> {
        let observed = match observed {
            ObservedPairAsk::Questions { call, .. } if call.questions.is_empty() => {
                let _ = call.reply.send(Vec::new());
                return Ok(());
            }
            observed => observed,
        };

        let Some(raw_id) = self.next_ask_id else {
            observed.fail_closed();
            return Err(PairAskObservationError::IdExhausted);
        };
        self.next_ask_id = raw_id.checked_add(1);
        let id = PairAskId(raw_id);
        let pending = match observed {
            ObservedPairAsk::Approval { peer, call } => PendingPairAsk::Approval {
                id,
                peer,
                request: call.request,
                reply: call.reply,
            },
            ObservedPairAsk::Questions { peer, call } => PendingPairAsk::Questions {
                id,
                peer,
                questions: call.questions,
                reply: call.reply,
            },
        };
        if self.active_ask.is_none() {
            self.active_ask = Some(pending);
            self.active_needs_emit = true;
        } else {
            self.queued_asks.push_back(pending);
        }
        Ok(())
    }

    fn promote_queued_ask(&mut self) {
        self.active_ask = self.queued_asks.pop_front();
        self.active_needs_emit = self.active_ask.is_some();
    }

    fn stop_for_ask_error(&mut self, error: PairAskObservationError) -> PairPumpEvent {
        self.fail_closed_asks();
        self.abort_driver_and_hosts();
        match error {
            PairAskObservationError::ChannelClosed(source) => PairPumpEvent::AskChannelClosed {
                peer: source.peer(),
                kind: source.kind(),
            },
            PairAskObservationError::IdExhausted => PairPumpEvent::InvariantViolation {
                detail: "user request identity space was exhausted".to_string(),
            },
        }
    }

    /// This order is load-bearing and the operation is intentionally idempotent:
    /// close new sends, wake owned requests, drain buffered sends, then fuse arms.
    fn fail_closed_asks(&mut self) {
        self.owner.a.approvals.close();
        self.owner.a.questions.close();
        self.owner.b.approvals.close();
        self.owner.b.questions.close();

        if let Some(active) = self.active_ask.take() {
            active.fail_closed();
        }
        for ask in self.queued_asks.drain(..) {
            ask.fail_closed();
        }
        while let Ok(call) = self.owner.a.approvals.try_recv() {
            deny_approval(call);
        }
        while let Ok(call) = self.owner.a.questions.try_recv() {
            dismiss_questions(call);
        }
        while let Ok(call) = self.owner.b.approvals.try_recv() {
            deny_approval(call);
        }
        while let Ok(call) = self.owner.b.questions.try_recv() {
            dismiss_questions(call);
        }

        self.active_needs_emit = false;
        self.ask_open = [false; 4];
    }

    /// Complete shutdown after the terminal owner has restored terminal modes.
    ///
    /// A live driver is signalled and awaited cooperatively; it is never aborted,
    /// detached, or dropped. Hosted ownership is then closed in lifecycle order.
    pub(crate) async fn shutdown(mut self) -> PairRunCompletion {
        if let Some(driver) = self.driver.take() {
            self.abort_and_cancel();
            self.record_driver_result(driver.await);
        } else {
            self.fail_closed_asks();
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
        self.fail_closed_asks();
        let (terminal, completion) = match result {
            Ok(report) => (
                terminal_status(report.reason()),
                PairRunCompletion::Report(report),
            ),
            Err(error) => {
                self.abort_driver_and_hosts();
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

fn deny_approval(call: ApprovalCall) {
    let _ = call.reply.send(false);
}

fn dismiss_questions(call: QuestionCall) {
    let _ = call
        .reply
        .send(vec![UserAnswer::Dismissed; call.questions.len()]);
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

    struct AskSenders {
        a_approvals: mpsc::UnboundedSender<ApprovalCall>,
        a_questions: mpsc::UnboundedSender<QuestionCall>,
        b_approvals: mpsc::UnboundedSender<ApprovalCall>,
        b_questions: mpsc::UnboundedSender<QuestionCall>,
    }

    fn replace_ask_receivers(run: &mut InteractivePairRun) -> AskSenders {
        let (a_approvals, a_approval_rx) = mpsc::unbounded_channel();
        let (a_questions, a_question_rx) = mpsc::unbounded_channel();
        let (b_approvals, b_approval_rx) = mpsc::unbounded_channel();
        let (b_questions, b_question_rx) = mpsc::unbounded_channel();
        run.owner.a.approvals = a_approval_rx;
        run.owner.a.questions = a_question_rx;
        run.owner.b.approvals = b_approval_rx;
        run.owner.b.questions = b_question_rx;
        AskSenders {
            a_approvals,
            a_questions,
            b_approvals,
            b_questions,
        }
    }

    fn approval_call(tool: &str) -> (ApprovalCall, oneshot::Receiver<bool>) {
        let (reply, answer) = oneshot::channel();
        (
            ApprovalCall {
                request: ApprovalRequest {
                    tool: tool.to_string(),
                    target: format!("{tool}-target"),
                    risk_class: format!("{tool}-risk"),
                },
                reply,
            },
            answer,
        )
    }

    fn question_call(
        label: &str,
        count: usize,
    ) -> (QuestionCall, oneshot::Receiver<Vec<UserAnswer>>) {
        let (reply, answer) = oneshot::channel();
        let questions = (0..count)
            .map(|index| UserQuestion {
                header: Some(format!("{label}-{index}")),
                question: format!("{label} question {index}"),
                options: Vec::new(),
                multi_select: false,
            })
            .collect();
        (QuestionCall { questions, reply }, answer)
    }

    async fn expect_ask(run: &mut InteractivePairRun) -> PairAsk {
        match run.next().await {
            Some(PairPumpEvent::Ask(ask)) => ask,
            other => panic!("expected an attributed request, got {other:?}"),
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
    async fn asks_are_attributed_queued_and_answer_only_their_origin() {
        let directory = tempfile::tempdir().unwrap();
        let setup = setup(
            directory.path(),
            PendingProvider::arc("first"),
            PendingProvider::arc("second"),
        );
        let mut run = PreparedPairRun::new(prepare(&setup).await, bounds(2))
            .unwrap()
            .spawn();
        let senders = replace_ask_receivers(&mut run);

        let (a_approval, a_approval_answer) = approval_call("a-approval");
        let (a_questions, mut a_question_answers) = question_call("a-questions", 1);
        let (b_approval, mut b_approval_answer) = approval_call("b-approval");
        let (b_questions, mut b_question_answers) = question_call("b-questions", 2);
        senders.a_approvals.send(a_approval).unwrap();
        senders.a_questions.send(a_questions).unwrap();
        senders.b_approvals.send(b_approval).unwrap();
        senders.b_questions.send(b_questions).unwrap();

        let ask = expect_ask(&mut run).await;
        assert_eq!(ask.peer, PairPeer::A);
        assert_eq!(run.queued_asks.len(), 3);
        assert_eq!(
            run.active_ask.as_ref().map(PendingPairAsk::id),
            Some(ask.id)
        );
        match &ask.request {
            PairAskRequest::Approval(request) => {
                assert_eq!(request.tool, "a-approval");
                assert_eq!(request.target, "a-approval-target");
            }
            other => panic!("unexpected first request: {other:?}"),
        }
        assert_eq!(
            run.answer_ask(PairAskId(999), PairAskAnswer::Approval(true)),
            Err(PairAskAnswerError::Stale {
                received: PairAskId(999),
                active: ask.id,
            })
        );
        assert_eq!(
            run.answer_ask(ask.id, PairAskAnswer::Questions(Vec::new())),
            Err(PairAskAnswerError::WrongKind {
                id: ask.id,
                expected: PairAskKind::Approval,
                received: PairAskKind::Questions,
            })
        );
        assert!(matches!(
            a_question_answers.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            b_approval_answer.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            b_question_answers.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        run.answer_ask(ask.id, PairAskAnswer::Approval(true))
            .unwrap();
        assert!(a_approval_answer.await.unwrap());

        let ask = expect_ask(&mut run).await;
        assert_eq!(ask.peer, PairPeer::A);
        let PairAskRequest::Questions(questions) = &ask.request else {
            panic!("second request was not a question call");
        };
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].header.as_deref(), Some("a-questions-0"));
        assert_eq!(
            run.answer_ask(ask.id, PairAskAnswer::Questions(Vec::new())),
            Err(PairAskAnswerError::WrongQuestionCount {
                id: ask.id,
                expected: 1,
                received: 0,
            })
        );
        let a_answers = vec![UserAnswer::Selected(vec!["first".to_string()])];
        run.answer_ask(ask.id, PairAskAnswer::Questions(a_answers.clone()))
            .unwrap();
        assert_eq!(a_question_answers.await.unwrap(), a_answers);

        let ask = expect_ask(&mut run).await;
        assert_eq!(ask.peer, PairPeer::B);
        assert!(matches!(ask.request, PairAskRequest::Approval(_)));
        run.answer_ask(ask.id, PairAskAnswer::Approval(false))
            .unwrap();
        assert!(!b_approval_answer.await.unwrap());

        let ask = expect_ask(&mut run).await;
        assert_eq!(ask.peer, PairPeer::B);
        let PairAskRequest::Questions(questions) = &ask.request else {
            panic!("fourth request was not a question call");
        };
        assert_eq!(questions.len(), 2);
        let b_answers = vec![
            UserAnswer::Dismissed,
            UserAnswer::Other("second".to_string()),
        ];
        run.answer_ask(ask.id, PairAskAnswer::Questions(b_answers.clone()))
            .unwrap();
        assert_eq!(b_question_answers.await.unwrap(), b_answers);

        let (orphaned, orphaned_answer) = approval_call("orphaned");
        drop(orphaned_answer);
        senders.a_approvals.send(orphaned).unwrap();
        let ask = expect_ask(&mut run).await;
        assert_eq!(
            run.answer_ask(ask.id, PairAskAnswer::Approval(false)),
            Err(PairAskAnswerError::RequesterGone { id: ask.id })
        );
        assert_eq!(
            run.answer_ask(ask.id, PairAskAnswer::Approval(false)),
            Err(PairAskAnswerError::NoActive { received: ask.id })
        );

        let (empty_questions, empty_answers) = question_call("empty", 0);
        senders.b_questions.send(empty_questions).unwrap();
        run.scan_buffered_asks().unwrap();
        assert!(empty_answers.await.unwrap().is_empty());
        assert!(run.active_ask.is_none());

        let completion = run.shutdown().await;
        assert!(matches!(
            completion.report().map(PairReport::reason),
            Some(PairOutcome::Aborted)
        ));
    }

    #[tokio::test]
    async fn safe_abort_defaults_active_queued_and_buffered_asks() {
        let directory = tempfile::tempdir().unwrap();
        let setup = setup(
            directory.path(),
            PendingProvider::arc("first"),
            PendingProvider::arc("second"),
        );
        let mut run = PreparedPairRun::new(prepare(&setup).await, bounds(2))
            .unwrap()
            .spawn();
        let sessions = run.owner.sessions;
        let senders = replace_ask_receivers(&mut run);

        let (active, active_answer) = approval_call("active");
        let (queued_questions, queued_question_answers) = question_call("queued-a", 1);
        let (queued_approval, queued_approval_answer) = approval_call("queued-b");
        let (queued_b_questions, queued_b_question_answers) = question_call("queued-b", 2);
        senders.a_approvals.send(active).unwrap();
        senders.a_questions.send(queued_questions).unwrap();
        senders.b_approvals.send(queued_approval).unwrap();
        senders.b_questions.send(queued_b_questions).unwrap();
        let visible = expect_ask(&mut run).await;
        assert_eq!(visible.peer, PairPeer::A);

        let (buffered_approval, buffered_approval_answer) = approval_call("buffered-a");
        let (buffered_questions, buffered_question_answers) = question_call("buffered-a", 2);
        let (buffered_b_approval, buffered_b_approval_answer) = approval_call("buffered-b");
        let (buffered_b_questions, buffered_b_question_answers) = question_call("buffered-b", 1);
        senders.a_approvals.send(buffered_approval).unwrap();
        senders.a_questions.send(buffered_questions).unwrap();
        senders.b_approvals.send(buffered_b_approval).unwrap();
        senders.b_questions.send(buffered_b_questions).unwrap();

        run.abort_and_cancel();
        assert!(!active_answer.await.unwrap());
        assert_eq!(
            queued_question_answers.await.unwrap(),
            vec![UserAnswer::Dismissed]
        );
        assert!(!queued_approval_answer.await.unwrap());
        assert_eq!(
            queued_b_question_answers.await.unwrap(),
            vec![UserAnswer::Dismissed; 2]
        );
        assert!(!buffered_approval_answer.await.unwrap());
        assert_eq!(
            buffered_question_answers.await.unwrap(),
            vec![UserAnswer::Dismissed; 2]
        );
        assert!(!buffered_b_approval_answer.await.unwrap());
        assert_eq!(
            buffered_b_question_answers.await.unwrap(),
            vec![UserAnswer::Dismissed]
        );

        let (after_close, _answer) = approval_call("after-close");
        assert!(senders.a_approvals.send(after_close).is_err());
        let (after_close, _answers) = question_call("after-close", 1);
        assert!(senders.b_questions.send(after_close).is_err());

        let completion = run.shutdown().await;
        assert!(matches!(
            completion.report().map(PairReport::reason),
            Some(PairOutcome::Aborted)
        ));
        for session in sessions {
            assert_session_closed(directory.path(), session);
        }
    }

    #[tokio::test]
    async fn ask_channel_closure_is_attributed_once_and_aborts_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let setup = setup(
            directory.path(),
            PendingProvider::arc("first"),
            PendingProvider::arc("second"),
        );
        let mut run = PreparedPairRun::new(prepare(&setup).await, bounds(2))
            .unwrap()
            .spawn();
        let AskSenders {
            a_approvals,
            a_questions,
            b_approvals,
            b_questions,
        } = replace_ask_receivers(&mut run);

        let (active, active_answer) = approval_call("closing-source");
        let (a_question, a_question_answers) = question_call("a", 1);
        let (b_approval, b_approval_answer) = approval_call("b");
        let (b_question, b_question_answers) = question_call("b", 2);
        a_approvals.send(active).unwrap();
        drop(a_approvals);
        a_questions.send(a_question).unwrap();
        b_approvals.send(b_approval).unwrap();
        b_questions.send(b_question).unwrap();

        let visible = expect_ask(&mut run).await;
        assert_eq!(visible.peer, PairPeer::A);
        assert!(matches!(visible.request, PairAskRequest::Approval(_)));
        assert!(matches!(
            run.next().await,
            Some(PairPumpEvent::AskChannelClosed {
                peer: PairPeer::A,
                kind: PairAskKind::Approval,
            })
        ));
        assert!(!active_answer.await.unwrap());
        assert_eq!(
            a_question_answers.await.unwrap(),
            vec![UserAnswer::Dismissed]
        );
        assert!(!b_approval_answer.await.unwrap());
        assert_eq!(
            b_question_answers.await.unwrap(),
            vec![UserAnswer::Dismissed; 2]
        );
        assert!(a_questions.send(question_call("closed", 1).0).is_err());
        assert!(b_approvals.send(approval_call("closed").0).is_err());
        assert!(b_questions.send(question_call("closed", 1).0).is_err());

        let terminal = loop {
            match run.next().await.unwrap() {
                PairPumpEvent::Runtime { .. }
                | PairPumpEvent::RuntimeLagged { .. }
                | PairPumpEvent::Progress(_) => {}
                PairPumpEvent::Finished(status) => break status,
                PairPumpEvent::AskChannelClosed { .. } => {
                    panic!("closed request channel emitted more than once")
                }
                other => panic!("unexpected shutdown event: {other:?}"),
            }
        };
        assert_eq!(
            terminal.state,
            PairRunState::Finished(PairTerminalStatus::Aborted)
        );
        assert!(run.next().await.is_none());
        let completion = run.shutdown().await;
        assert!(matches!(
            completion.report().map(PairReport::reason),
            Some(PairOutcome::Aborted)
        ));
    }

    #[tokio::test]
    async fn driver_failure_defaults_asks_before_the_terminal_event() {
        let directory = tempfile::tempdir().unwrap();
        let setup = setup(
            directory.path(),
            PendingProvider::arc("first"),
            PendingProvider::arc("second"),
        );
        let mut run = PreparedPairRun::new(prepare(&setup).await, bounds(2))
            .unwrap()
            .spawn();
        let senders = replace_ask_receivers(&mut run);
        let (approval, approval_answer) = approval_call("active");
        let (questions, question_answers) = question_call("queued", 2);
        senders.a_approvals.send(approval).unwrap();
        senders.a_questions.send(questions).unwrap();
        let visible = expect_ask(&mut run).await;
        assert!(matches!(visible.request, PairAskRequest::Approval(_)));

        run.abort_driver_and_hosts();
        let original = run.driver.take().unwrap();
        let _report = original.await.unwrap();
        run.driver = Some(tokio::spawn(async move {
            panic!("forced driver task failure");
        }));

        let detail = loop {
            match run.next().await.unwrap() {
                PairPumpEvent::Runtime { .. }
                | PairPumpEvent::RuntimeLagged { .. }
                | PairPumpEvent::Progress(_) => {}
                PairPumpEvent::DriverFailed { detail, .. } => break detail,
                PairPumpEvent::Ask(_) => panic!("a defaulted request reached the final screen"),
                other => panic!("unexpected driver-failure event: {other:?}"),
            }
        };
        assert!(detail.contains("forced driver task failure"));
        assert!(!approval_answer.await.unwrap());
        assert_eq!(
            question_answers.await.unwrap(),
            vec![UserAnswer::Dismissed; 2]
        );
        assert!(run.next().await.is_none());
        assert!(matches!(
            run.shutdown().await,
            PairRunCompletion::DriverFailed(_)
        ));
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
