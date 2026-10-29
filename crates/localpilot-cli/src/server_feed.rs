//! The served session's sequenced event feed.
//!
//! Each session hosted by `localpilot serve` has one [`SessionFeed`]. A single
//! pump task is its only writer: it reads the session's runtime events and its
//! permission asks, numbers every record, keeps recent records in a bounded
//! ring, and applies each one to a projection of the session (turn state, plan,
//! current-turn text, pending asks) under the same lock. A connection reads
//! that one numbered stream, so every attached client sees the same records in
//! the same order, and a reconnecting client either replays what it missed or
//! receives the projection as a snapshot taken exactly at a feed position.
//!
//! When the pump itself falls behind a source it cannot know what it missed:
//! it drops its backlog, rebuilds the pending asks from the registry, starts a
//! new epoch, marks the turn state, plan and text unknown, and announces
//! `resync_required`. Nothing derived from lost events is presented as known.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use localpilot_core::{ContentBlock, EventId, Message, Role};
use localpilot_harness::RuntimeEvent;
use localpilot_rpc::{
    map_event, AskDetail, AskEvent, AskRegistry, FeedCursor, HistoryMessage, PlanStepWire, SeqRing,
    ServerEvent, TurnState,
};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// Records a reconnecting client can replay.
const FEED_CAPACITY: usize = 2048;
/// Live records buffered per connection before it lags.
const LIVE_CAPACITY: usize = 1024;
/// The current turn's text kept for a snapshot (its tail).
const TURN_TEXT_CAP: usize = 64 * 1024;
/// Stored transcript messages and bytes a snapshot carries.
const HISTORY_MESSAGES: usize = 20;
const HISTORY_BYTES: usize = 64 * 1024;

/// One numbered record, as delivered to connections.
pub(crate) type FeedRecord = (u64, ServerEvent);

/// What the projection knows about the session at the feed's head.
struct Projection {
    turn: TurnState,
    plan: Option<Vec<PlanStepWire>>,
    /// `None` when the current turn's text was lost.
    turn_text: Option<String>,
    turn_text_truncated: bool,
    asks: BTreeMap<String, AskDetail>,
}

impl Projection {
    fn new() -> Self {
        Self {
            turn: TurnState::Idle,
            plan: None,
            turn_text: Some(String::new()),
            turn_text_truncated: false,
            asks: BTreeMap::new(),
        }
    }

    fn apply(&mut self, event: &ServerEvent) {
        match event {
            ServerEvent::TurnStarted => {
                self.turn = TurnState::Running;
                self.plan = None;
                self.turn_text = Some(String::new());
                self.turn_text_truncated = false;
            }
            ServerEvent::TextDelta { text } => {
                if let Some(turn_text) = &mut self.turn_text {
                    turn_text.push_str(text);
                    if keep_tail(turn_text, TURN_TEXT_CAP) {
                        self.turn_text_truncated = true;
                    }
                }
            }
            ServerEvent::Plan { steps } => self.plan = Some(steps.clone()),
            ServerEvent::Stopped { .. } => self.turn = TurnState::Idle,
            ServerEvent::PermissionAsk {
                ask_id,
                tool,
                detail,
                risk,
                expires_at_ms,
            } => {
                self.asks.insert(
                    ask_id.clone(),
                    AskDetail {
                        ask_id: ask_id.clone(),
                        tool: tool.clone(),
                        detail: detail.clone(),
                        risk: risk.clone(),
                        expires_at_ms: *expires_at_ms,
                    },
                );
            }
            ServerEvent::PermissionResolved { ask_id, .. } => {
                self.asks.remove(ask_id);
            }
            _ => {}
        }
    }
}

/// Trim `text` to at most `cap` bytes, keeping its end on a char boundary.
/// Returns whether anything was dropped.
fn keep_tail(text: &mut String, cap: usize) -> bool {
    if text.len() <= cap {
        return false;
    }
    let mut start = text.len() - cap;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text.drain(..start);
    true
}

struct FeedState {
    epoch: String,
    ring: SeqRing<ServerEvent>,
    projection: Projection,
    /// Set once, by [`SessionFeed::close`]: why the session closed. A closed
    /// feed takes no more records, and every attach or resync sees it.
    closed: Option<String>,
}

/// The session host's admission state, read under the feed lock when a
/// snapshot is taken: whether accepted work is running or about to start, and
/// how many follow-ups wait. It is current, not bound to the feed position.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Admission {
    pub(crate) busy: bool,
    pub(crate) queued: usize,
}

/// Where a feed stands: its epoch, its head, and whether it has closed.
pub(crate) struct Position {
    pub(crate) epoch: String,
    pub(crate) head: u64,
    pub(crate) closed: Option<String>,
}

/// What a connection receives when it attaches.
pub(crate) struct Attachment {
    pub(crate) live: broadcast::Receiver<FeedRecord>,
    pub(crate) epoch: String,
    /// Records to send before any live one: a replay, a snapshot, or (for
    /// clients without `resume`) the outstanding asks.
    pub(crate) initial: Vec<(Option<u64>, ServerEvent)>,
    /// Live records at or below this position are already covered by `initial`.
    pub(crate) floor: u64,
    pub(crate) guard: ClientGuard,
}

/// Counts one attached connection while it lives.
pub(crate) struct ClientGuard(Arc<SessionFeed>);

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.clients.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One served session's sequenced feed. See the module docs.
pub(crate) struct SessionFeed {
    state: Mutex<FeedState>,
    live: broadcast::Sender<FeedRecord>,
    clients: AtomicUsize,
    cancel: CancellationToken,
}

impl SessionFeed {
    /// Create the feed for a session and start its pump. The receivers are
    /// taken here, before the session can run a turn, so the pump misses
    /// nothing.
    pub(crate) fn start(
        runtime: broadcast::Receiver<RuntimeEvent>,
        asks: Option<AskRegistry>,
    ) -> Arc<Self> {
        let ask_rx = asks.as_ref().map(AskRegistry::subscribe);
        let feed = Arc::new(Self::new(FEED_CAPACITY));
        if let Some(asks) = &asks {
            feed.rebuild_asks(asks.outstanding_detail(), false);
        }
        tokio::spawn(pump(feed.clone(), runtime, asks, ask_rx));
        feed
    }

    fn new(capacity: usize) -> Self {
        let (live, _) = broadcast::channel(LIVE_CAPACITY);
        Self {
            state: Mutex::new(FeedState {
                epoch: new_epoch(),
                ring: SeqRing::new(capacity),
                projection: Projection::new(),
                closed: None,
            }),
            live,
            clients: AtomicUsize::new(0),
            cancel: CancellationToken::new(),
        }
    }

    fn state(&self) -> MutexGuard<'_, FeedState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How many connections are attached.
    pub(crate) fn clients(&self) -> usize {
        self.clients.load(Ordering::SeqCst)
    }

    /// Number `event`, apply it to the projection and deliver it, all under
    /// the feed lock so the projection always matches the head.
    fn push(&self, event: ServerEvent) {
        let mut state = self.state();
        if state.closed.is_some() {
            return; // terminal: nothing follows `session_closed`
        }
        state.projection.apply(&event);
        let seq = state.ring.push(event.clone());
        let _ = self.live.send((seq, event));
    }

    /// The pump lost source events: start a new epoch whose projection holds
    /// only what is still authoritative.
    fn rebuild_asks(&self, outstanding: Vec<localpilot_rpc::PendingAsk>, lost_runtime: bool) {
        let mut state = self.state();
        if state.closed.is_some() {
            return;
        }
        state.projection.asks = outstanding
            .iter()
            .map(|ask| (ask.ask_id.clone(), ask.detail()))
            .collect();
        if lost_runtime {
            state.epoch = new_epoch();
            state.ring.clear();
            state.projection.turn = TurnState::Unknown;
            state.projection.plan = None;
            state.projection.turn_text = None;
            state.projection.turn_text_truncated = false;
            let epoch = state.epoch.clone();
            let seq = state.ring.head() + 1;
            let event = ServerEvent::ResyncRequired { epoch, seq };
            let seq = state.ring.push(event.clone());
            let _ = self.live.send((seq, event));
        }
    }

    /// Attach a connection. Under the feed lock: subscribe to live records,
    /// then decide what the connection needs first, so nothing is missed or
    /// sent twice. A closed feed hands back only its closure.
    pub(crate) fn attach(
        self: &Arc<Self>,
        since: Option<&FeedCursor>,
        resume: bool,
        history: &(Vec<HistoryMessage>, bool),
        admission: &dyn Fn() -> Admission,
    ) -> Attachment {
        let state = self.state();
        let live = self.live.subscribe();
        self.clients.fetch_add(1, Ordering::SeqCst);
        let guard = ClientGuard(self.clone());
        let head = state.ring.head();
        let initial = if let Some(reason) = &state.closed {
            vec![(
                None,
                ServerEvent::SessionClosed {
                    reason: reason.clone(),
                },
            )]
        } else {
            let replay = since
                .filter(|cursor| resume && cursor.epoch == state.epoch)
                .and_then(|cursor| state.ring.complete_after(cursor.seq));
            match replay {
                Some(entries) => entries
                    .into_iter()
                    .map(|(seq, event)| (Some(seq), event))
                    .collect(),
                None if resume => vec![(None, snapshot(&state, history, admission()))],
                None => state
                    .projection
                    .asks
                    .values()
                    .map(|ask| (None, ask_event(ask)))
                    .collect(),
            }
        };
        Attachment {
            live,
            epoch: state.epoch.clone(),
            initial,
            floor: head,
            guard,
        }
    }

    /// A snapshot at the current head, and that head; or the closure, when
    /// the feed has closed.
    pub(crate) fn snapshot(
        &self,
        history: &(Vec<HistoryMessage>, bool),
        admission: &dyn Fn() -> Admission,
    ) -> (ServerEvent, u64) {
        let state = self.state();
        let event = match &state.closed {
            Some(reason) => ServerEvent::SessionClosed {
                reason: reason.clone(),
            },
            None => snapshot(&state, history, admission()),
        };
        (event, state.ring.head())
    }

    /// The pending asks as the projection knows them, with the feed's position.
    pub(crate) fn pending_asks(&self) -> (Vec<AskDetail>, Position) {
        let state = self.state();
        (
            state.projection.asks.values().cloned().collect(),
            Position {
                epoch: state.epoch.clone(),
                head: state.ring.head(),
                closed: state.closed.clone(),
            },
        )
    }

    /// The session is going away: record the closure (terminal), tell every
    /// connection, then stop the pump.
    pub(crate) fn close(&self, reason: &str) {
        {
            let mut state = self.state();
            if state.closed.is_none() {
                let event = ServerEvent::SessionClosed {
                    reason: reason.to_string(),
                };
                let seq = state.ring.push(event.clone());
                state.closed = Some(reason.to_string());
                let _ = self.live.send((seq, event));
            }
        }
        self.cancel.cancel();
    }
}

fn new_epoch() -> String {
    EventId::new().to_string()
}

fn ask_event(ask: &AskDetail) -> ServerEvent {
    ServerEvent::PermissionAsk {
        ask_id: ask.ask_id.clone(),
        tool: ask.tool.clone(),
        detail: ask.detail.clone(),
        risk: ask.risk.clone(),
        expires_at_ms: ask.expires_at_ms,
    }
}

fn snapshot(
    state: &FeedState,
    history: &(Vec<HistoryMessage>, bool),
    admission: Admission,
) -> ServerEvent {
    let projection = &state.projection;
    ServerEvent::Snapshot {
        epoch: state.epoch.clone(),
        seq: state.ring.head(),
        turn: projection.turn,
        busy: admission.busy || projection.turn == TurnState::Running,
        queued: admission.queued,
        plan: projection.plan.clone(),
        asks: projection.asks.values().cloned().collect(),
        turn_text: projection.turn_text.clone(),
        turn_text_truncated: projection.turn_text_truncated,
        history: history.0.clone(),
        history_truncated: history.1,
    }
}

/// The stored transcript tail for a snapshot: the last messages with text,
/// bounded in count and bytes. Read from the store, never under the feed lock.
pub(crate) fn stored_history(transcript: &[Message]) -> (Vec<HistoryMessage>, bool) {
    let mut out = Vec::new();
    let mut bytes = 0;
    let mut truncated = false;
    for message in transcript.iter().rev() {
        let text: String = message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            continue;
        }
        if out.len() == HISTORY_MESSAGES || bytes + text.len() > HISTORY_BYTES {
            truncated = true;
            break;
        }
        bytes += text.len();
        out.push(HistoryMessage {
            role: role_label(message.role).to_string(),
            text,
        });
    }
    out.reverse();
    (out, truncated)
}

fn role_label(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::System => "system",
        _ => "other",
    }
}

/// The feed's only writer. Ends when the session's runtime stream closes or
/// the feed is closed.
async fn pump(
    feed: Arc<SessionFeed>,
    mut runtime: broadcast::Receiver<RuntimeEvent>,
    asks: Option<AskRegistry>,
    mut ask_rx: Option<broadcast::Receiver<AskEvent>>,
) {
    loop {
        tokio::select! {
            biased;
            () = feed.cancel.cancelled() => break,
            event = runtime.recv() => match event {
                Ok(RuntimeEvent::TurnStarted) => feed.push(ServerEvent::TurnStarted),
                Ok(event) => {
                    if let Some(mapped) = map_event(event) {
                        feed.push(mapped);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    resubscribe(&mut runtime, &mut ask_rx);
                    rebuild(&feed, asks.as_ref());
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            event = next_ask(&mut ask_rx) => match event {
                Ok(AskEvent::Raised(ask)) => {
                    // After a rebuild the ask may already be known.
                    let known = feed.state().projection.asks.contains_key(&ask.ask_id);
                    if !known {
                        feed.push(ask.wire_event());
                    }
                }
                Ok(AskEvent::Resolved { ask_id, outcome, by }) => {
                    feed.push(ServerEvent::PermissionResolved { ask_id, outcome, by });
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    resubscribe(&mut runtime, &mut ask_rx);
                    rebuild(&feed, asks.as_ref());
                }
                Err(broadcast::error::RecvError::Closed) => ask_rx = None,
            },
        }
    }
}

/// Drop every queued source event: nothing received before the rebuild may be
/// applied after it.
fn resubscribe(
    runtime: &mut broadcast::Receiver<RuntimeEvent>,
    ask_rx: &mut Option<broadcast::Receiver<AskEvent>>,
) {
    *runtime = runtime.resubscribe();
    if let Some(rx) = ask_rx {
        *rx = rx.resubscribe();
    }
}

fn rebuild(feed: &SessionFeed, asks: Option<&AskRegistry>) {
    let outstanding = asks
        .map(AskRegistry::outstanding_detail)
        .unwrap_or_default();
    feed.rebuild_asks(outstanding, true);
}

async fn next_ask(
    rx: &mut Option<broadcast::Receiver<AskEvent>>,
) -> Result<AskEvent, broadcast::error::RecvError> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn text(t: &str) -> ServerEvent {
        ServerEvent::TextDelta {
            text: t.to_string(),
        }
    }

    fn stopped() -> ServerEvent {
        ServerEvent::Stopped {
            reason: "done".to_string(),
        }
    }

    fn no_history() -> (Vec<HistoryMessage>, bool) {
        (Vec::new(), false)
    }

    fn idle() -> Admission {
        Admission::default()
    }

    #[test]
    fn a_closed_feed_is_terminal_for_attach_and_snapshot() {
        let feed = Arc::new(SessionFeed::new(16));
        feed.push(ServerEvent::TurnStarted);
        let mut live = feed.live.subscribe();
        feed.close("closed after inactivity");
        feed.close("again");
        feed.push(text("too late"));

        // Exactly one closure record, and nothing after it.
        assert!(matches!(
            live.try_recv().unwrap().1,
            ServerEvent::SessionClosed { reason } if reason == "closed after inactivity"
        ));
        assert!(live.try_recv().is_err());

        // A connection that attaches after the close gets only the closure,
        // whatever it asked for.
        for resume in [true, false] {
            let attached = feed.attach(None, resume, &no_history(), &idle);
            assert_eq!(attached.initial.len(), 1);
            assert!(matches!(
                attached.initial[0].1,
                ServerEvent::SessionClosed { .. }
            ));
        }
        // A resync after the close yields the closure, not a snapshot.
        assert!(matches!(
            feed.snapshot(&no_history(), &idle).0,
            ServerEvent::SessionClosed { .. }
        ));
    }

    #[test]
    fn a_snapshot_reports_accepted_work_as_busy_before_its_turn_starts() {
        let feed = Arc::new(SessionFeed::new(16));
        let accepted = || Admission {
            busy: true,
            queued: 1,
        };
        match feed.snapshot(&no_history(), &accepted).0 {
            ServerEvent::Snapshot {
                turn, busy, queued, ..
            } => {
                assert_eq!(turn, TurnState::Idle, "no turn has started yet");
                assert!(busy, "but accepted work is starting");
                assert_eq!(queued, 1);
            }
            other => panic!("expected a snapshot, got {other:?}"),
        }
    }

    fn snapshot_parts(event: ServerEvent) -> (TurnState, Option<String>, Vec<AskDetail>, u64) {
        match event {
            ServerEvent::Snapshot {
                turn,
                turn_text,
                asks,
                seq,
                ..
            } => (turn, turn_text, asks, seq),
            other => panic!("expected a snapshot, got {other:?}"),
        }
    }

    #[test]
    fn a_snapshot_is_the_projection_at_the_head() {
        let feed = Arc::new(SessionFeed::new(16));
        feed.push(ServerEvent::TurnStarted);
        feed.push(text("hello "));
        feed.push(text("world"));
        let (event, head) = feed.snapshot(&no_history(), &idle);
        let (turn, turn_text, _, seq) = snapshot_parts(event);
        assert_eq!(turn, TurnState::Running);
        assert_eq!(turn_text.as_deref(), Some("hello world"));
        assert_eq!(seq, head);
        assert_eq!(head, 3);
    }

    #[test]
    fn two_turns_keep_their_own_text() {
        // Both turns complete before anyone reads: each delta still belongs to
        // the turn whose start precedes it in the one ordered stream.
        let feed = Arc::new(SessionFeed::new(16));
        for event in [
            ServerEvent::TurnStarted,
            text("first"),
            stopped(),
            ServerEvent::TurnStarted,
            text("second"),
            stopped(),
        ] {
            feed.push(event);
        }
        let (turn, turn_text, _, _) = snapshot_parts(feed.snapshot(&no_history(), &idle).0);
        assert_eq!(turn, TurnState::Idle);
        assert_eq!(turn_text.as_deref(), Some("second"));
    }

    #[test]
    fn a_cursor_in_the_same_epoch_replays_and_others_get_a_snapshot() {
        let feed = Arc::new(SessionFeed::new(4));
        feed.push(ServerEvent::TurnStarted);
        feed.push(text("a"));
        let epoch = feed.state().epoch.clone();

        let cursor = FeedCursor {
            epoch: epoch.clone(),
            seq: 1,
        };
        let attached = feed.attach(Some(&cursor), true, &no_history(), &idle);
        assert_eq!(attached.initial.len(), 1, "replays only record 2");
        assert_eq!(attached.initial[0].0, Some(2));

        let stale = FeedCursor {
            epoch: "another".to_string(),
            seq: 1,
        };
        let attached = feed.attach(Some(&stale), true, &no_history(), &idle);
        assert!(matches!(
            attached.initial[0].1,
            ServerEvent::Snapshot { .. }
        ));

        let ahead = FeedCursor { epoch, seq: 99 };
        let attached = feed.attach(Some(&ahead), true, &no_history(), &idle);
        assert!(
            matches!(attached.initial[0].1, ServerEvent::Snapshot { .. }),
            "a cursor ahead of the head is reset"
        );
        assert_eq!(feed.clients(), 3);
        drop(attached);
        assert_eq!(feed.clients(), 2);
    }

    #[test]
    fn a_rebuild_marks_runtime_state_unknown_and_keeps_authoritative_asks() {
        let feed = Arc::new(SessionFeed::new(16));
        feed.push(ServerEvent::TurnStarted);
        feed.push(text("partial"));
        feed.push(ServerEvent::Plan { steps: Vec::new() });
        let before = feed.state().epoch.clone();

        let ask = localpilot_rpc::PendingAsk {
            ask_id: "ask-1".to_string(),
            tool: "run_shell".to_string(),
            detail: "ls".to_string(),
            risk: "run a command".to_string(),
            expires_at_ms: 1,
        };
        let mut live = feed.live.subscribe();
        feed.rebuild_asks(vec![ask], true);

        let (turn, turn_text, asks, _) = snapshot_parts(feed.snapshot(&no_history(), &idle).0);
        assert_eq!(turn, TurnState::Unknown, "unknown, never idle");
        assert_eq!(turn_text, None, "lost text is not presented");
        assert_eq!(asks.len(), 1);
        assert_ne!(feed.state().epoch, before, "a new epoch");
        assert!(matches!(
            live.try_recv().unwrap().1,
            ServerEvent::ResyncRequired { .. }
        ));
        // A later delta alone does not make the interrupted turn's text known.
        feed.push(text(" more"));
        let (_, turn_text, _, _) = snapshot_parts(feed.snapshot(&no_history(), &idle).0);
        assert_eq!(turn_text, None);
        // A fresh turn start does.
        feed.push(ServerEvent::TurnStarted);
        let (turn, turn_text, _, _) = snapshot_parts(feed.snapshot(&no_history(), &idle).0);
        assert_eq!(turn, TurnState::Running);
        assert_eq!(turn_text.as_deref(), Some(""));
    }

    fn write_request(path: &str) -> localpilot_sandbox::PermissionRequest {
        localpilot_sandbox::PermissionRequest {
            tool: "write_file".to_string(),
            effect: localpilot_sandbox::Effect::WritePath {
                inside_workspace: true,
                secret_like: false,
                overwrite: false,
            },
            interactivity: localpilot_sandbox::Interactivity::Interactive,
            trusted: true,
            detail: path.to_string(),
        }
    }

    /// Raise an ask through a real approver; the approval runs on its own task.
    fn raise(approver: &'static localpilot_rpc::RpcApprover, path: &str) {
        use localpilot_sandbox::Approver as _;
        let request: &'static localpilot_sandbox::PermissionRequest =
            Box::leak(Box::new(write_request(path)));
        tokio::spawn(approver.approve(request));
    }

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pump_that_lags_its_source_rebuilds_from_the_registry() {
        let (approver, _ask_rx, registry) = localpilot_rpc::RpcApprover::new();
        let approver: &'static localpilot_rpc::RpcApprover = Box::leak(Box::new(approver));
        // A tiny runtime channel, overfilled before the pump reads it.
        let (tx, rx) = broadcast::channel(2);
        let feed = SessionFeed::start(rx, Some(registry.clone()));
        let mut live = feed.live.subscribe();
        let epoch_before = feed.state().epoch.clone();

        // Asks raised and resolved around the overflow.
        raise(approver, "a.md");
        settle().await;
        let first = registry.outstanding()[0].clone();
        tx.send(RuntimeEvent::TurnStarted).unwrap();
        for n in 0..5 {
            tx.send(RuntimeEvent::Text(format!("t{n}"))).unwrap();
        }
        assert!(registry
            .reply(&first, true, localpilot_rpc::Principal::Local)
            .is_ok());
        raise(approver, "b.md");
        settle().await;

        let (event, _) = feed.snapshot(&no_history(), &idle);
        let (turn, turn_text, asks, _) = snapshot_parts(event);
        let mut listed: Vec<String> = asks.into_iter().map(|ask| ask.ask_id).collect();
        listed.sort();
        let mut pending = registry.outstanding();
        pending.sort();
        assert_eq!(listed, pending, "pending asks equal the registry's");
        assert!(
            !listed.contains(&first),
            "a resolved ask is not resurrected"
        );
        assert_eq!(turn, TurnState::Unknown);
        assert_eq!(turn_text, None);
        assert_ne!(feed.state().epoch, epoch_before);
        let mut saw_resync = false;
        while let Ok((_, event)) = live.try_recv() {
            saw_resync |= matches!(event, ServerEvent::ResyncRequired { .. });
        }
        assert!(saw_resync, "the loss was announced");
        feed.close("test over");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closing_the_feed_stops_its_pump() {
        let (_tx, rx) = broadcast::channel::<RuntimeEvent>(4);
        let feed = SessionFeed::start(rx, None);
        assert!(Arc::strong_count(&feed) >= 2, "the pump holds the feed");
        let mut live = feed.live.subscribe();
        feed.close("closed after inactivity");
        assert!(matches!(
            live.try_recv().unwrap().1,
            ServerEvent::SessionClosed { .. }
        ));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while Arc::strong_count(&feed) > 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the pump exited");
    }

    #[test]
    fn turn_text_keeps_a_bounded_utf8_tail() {
        let mut text = "é".repeat(10);
        assert!(keep_tail(&mut text, 7));
        assert!(text.len() <= 7);
        assert!(text.chars().all(|c| c == 'é'));
    }

    #[test]
    fn stored_history_is_bounded_and_keeps_order() {
        let messages: Vec<Message> = (0..30)
            .map(|n| Message {
                role: if n % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                content: vec![ContentBlock::Text {
                    text: format!("m{n}"),
                }],
                metadata: Default::default(),
            })
            .collect();
        let (history, truncated) = stored_history(&messages);
        assert!(truncated);
        assert_eq!(history.len(), HISTORY_MESSAGES);
        assert_eq!(history.last().unwrap().text, "m29");
        assert_eq!(history.first().unwrap().text, "m10");
    }
}
