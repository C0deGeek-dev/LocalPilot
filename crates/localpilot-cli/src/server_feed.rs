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
/// Live records a byte-bounded feed buffers (with [`BOUNDED_RECORD_MAX`],
/// at most 8 MiB held for a lagging reader).
const BOUNDED_LIVE_CAPACITY: usize = 256;
/// The largest encoded record a byte-bounded feed keeps or delivers. A larger
/// permission ask or plan makes the feed unshareable until it is gone (the
/// local ask is untouched); any other larger record is replaced by a notice.
pub(crate) const BOUNDED_RECORD_MAX: usize = 32 * 1024;
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
    /// Pending asks too large to keep (byte-bounded feeds): while any is
    /// pending the feed cannot be shared.
    oversized_asks: std::collections::BTreeSet<String>,
    /// The current plan was too large to keep (byte-bounded feeds).
    oversized_plan: bool,
}

impl Projection {
    fn new() -> Self {
        Self {
            turn: TurnState::Idle,
            plan: None,
            turn_text: Some(String::new()),
            turn_text_truncated: false,
            asks: BTreeMap::new(),
            oversized_asks: std::collections::BTreeSet::new(),
            oversized_plan: false,
        }
    }

    fn apply(&mut self, event: &ServerEvent) {
        match event {
            ServerEvent::TurnStarted => {
                self.turn = TurnState::Running;
                self.plan = None;
                self.oversized_plan = false;
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
                self.oversized_asks.remove(ask_id);
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
    /// For a mirror: what its source's latest snapshot said beyond the
    /// projection. Kept under the same lock as the projection, so a snapshot
    /// of the mirror is always one consistent state.
    mirror: Option<MirrorExtra>,
    /// For a source feed (a session this process runs): the history tail and
    /// admission its host last published, kept under the same lock so a
    /// snapshot pairs them with the projection and position they belong to.
    source: Option<MirrorExtra>,
    /// How many times this feed lost records and rebuilt. Read with each
    /// snapshot, so a reader can tell a loss from an intentional refresh.
    losses: u64,
}

/// A mirror's history tail and admission state, as its source last reported
/// them.
#[derive(Debug, Clone, Default)]
struct MirrorExtra {
    history: (Vec<HistoryMessage>, bool),
    admission: Admission,
}

impl MirrorExtra {
    /// Nothing known: no history (marked incomplete), nothing running.
    fn unknown() -> Self {
        Self {
            history: (Vec::new(), true),
            admission: Admission::default(),
        }
    }
}

/// The session host's admission state, read under the feed lock when a
/// snapshot is taken: whether accepted work is running or about to start, and
/// how many follow-ups wait. It is current, not bound to the feed position.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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
    /// Counts each change to a source feed's published history or admission
    /// (see [`SessionFeed::source_changes`]).
    history: tokio::sync::watch::Sender<u64>,
}

impl SessionFeed {
    /// Create the feed for a session and start its pump. The receivers are
    /// taken here, before the session can run a turn, so the pump misses
    /// nothing.
    pub(crate) fn start(
        runtime: broadcast::Receiver<RuntimeEvent>,
        asks: Option<AskRegistry>,
    ) -> Arc<Self> {
        Self::start_with(runtime, asks, usize::MAX)
    }

    /// A byte-bounded feed whose records arrive through `ingress` (already
    /// bounded per record by [`bounded_records`]) rather than a broadcast
    /// subscription.
    pub(crate) fn start_fed(
        ingress: Arc<Ingress>,
        asks: Option<AskRegistry>,
        max_bytes: usize,
    ) -> Arc<Self> {
        let ask_rx = asks.as_ref().map(AskRegistry::subscribe);
        let feed = Arc::new(Self::new(FEED_CAPACITY, max_bytes));
        if let Some(asks) = &asks {
            feed.rebuild_asks(asks.outstanding_detail(), false);
        }
        tokio::spawn(pump_ingress(feed.clone(), ingress, asks, ask_rx));
        feed
    }

    fn start_with(
        runtime: broadcast::Receiver<RuntimeEvent>,
        asks: Option<AskRegistry>,
        max_bytes: usize,
    ) -> Arc<Self> {
        let ask_rx = asks.as_ref().map(AskRegistry::subscribe);
        let feed = Arc::new(Self::new(FEED_CAPACITY, max_bytes));
        if let Some(asks) = &asks {
            feed.rebuild_asks(asks.outstanding_detail(), false);
        }
        tokio::spawn(pump(feed.clone(), runtime, asks, ask_rx));
        feed
    }

    fn new(capacity: usize, max_bytes: usize) -> Self {
        let live_capacity = if max_bytes == usize::MAX {
            LIVE_CAPACITY
        } else {
            BOUNDED_LIVE_CAPACITY
        };
        let (live, _) = broadcast::channel(live_capacity);
        Self {
            state: Mutex::new(FeedState {
                epoch: new_epoch(),
                ring: SeqRing::with_byte_budget(capacity, max_bytes),
                projection: Projection::new(),
                closed: None,
                mirror: None,
                source: None,
                losses: 0,
            }),
            live,
            clients: AtomicUsize::new(0),
            cancel: CancellationToken::new(),
            history: tokio::sync::watch::channel(0).0,
        }
    }

    fn state(&self) -> MutexGuard<'_, FeedState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A feed with no pump of its own: it mirrors another process's session
    /// feed (a relayed session). It starts unknown (no turn state, no text, no
    /// asks) until [`SessionFeed::install`] gives it a verified snapshot.
    pub(crate) fn mirror() -> Arc<Self> {
        let feed = Arc::new(Self::new(FEED_CAPACITY, usize::MAX));
        {
            let mut state = feed.state();
            state.projection.turn = TurnState::Unknown;
            state.projection.turn_text = None;
            state.mirror = Some(MirrorExtra::unknown());
        }
        feed
    }

    /// Replace a mirror's whole state with a verified snapshot from its source,
    /// atomically, under one lock: a new epoch, an empty ring, the snapshot's
    /// projection, history tail and admission, and only then a
    /// `resync_required` so every attached connection reconciles against the
    /// complete new state. Returns whether it installed (anything but a
    /// snapshot, or a closed feed, leaves the mirror unchanged).
    pub(crate) fn install(&self, snapshot: &ServerEvent) -> bool {
        let ServerEvent::Snapshot {
            turn,
            busy,
            queued,
            plan,
            asks,
            turn_text,
            turn_text_truncated,
            history,
            history_truncated,
            ..
        } = snapshot
        else {
            return false;
        };
        let mut state = self.state();
        if state.closed.is_some() {
            return false;
        }
        state.epoch = new_epoch();
        state.ring.clear();
        state.projection = Projection {
            turn: *turn,
            plan: plan.clone(),
            turn_text: turn_text.clone(),
            turn_text_truncated: *turn_text_truncated,
            asks: asks
                .iter()
                .map(|ask| (ask.ask_id.clone(), ask.clone()))
                .collect(),
            ..Projection::new()
        };
        state.mirror = Some(MirrorExtra {
            history: (history.clone(), *history_truncated),
            admission: Admission {
                busy: *busy,
                queued: *queued,
            },
        });
        let event = ServerEvent::ResyncRequired {
            epoch: state.epoch.clone(),
            seq: state.ring.head() + 1,
        };
        let seq = state.ring.push(event.clone());
        let _ = self.live.send((seq, event));
        true
    }

    /// A mirror lost its source's order (a gap, or a snapshot that did not
    /// verify): until the next install it knows nothing — no turn state, no
    /// text, no asks — and every connection is told to reconcile.
    pub(crate) fn forget(&self) {
        let mut state = self.state();
        if state.closed.is_some() {
            return;
        }
        state.epoch = new_epoch();
        state.ring.clear();
        state.projection.turn = TurnState::Unknown;
        state.projection.plan = None;
        state.projection.turn_text = None;
        state.projection.turn_text_truncated = false;
        state.projection.asks.clear();
        if state.mirror.is_some() {
            state.mirror = Some(MirrorExtra::unknown());
        }
        let event = ServerEvent::ResyncRequired {
            epoch: state.epoch.clone(),
            seq: state.ring.head() + 1,
        };
        let seq = state.ring.push(event.clone());
        let _ = self.live.send((seq, event));
    }

    /// Append one record relayed from the mirror's source, in the source's
    /// order.
    pub(crate) fn relay(&self, event: ServerEvent) {
        self.push(event);
    }

    /// Whether everything the feed's state holds can be shared (no pending ask
    /// or current plan was too large to keep).
    pub(crate) fn shareable(&self) -> bool {
        let state = self.state();
        state.projection.oversized_asks.is_empty() && !state.projection.oversized_plan
    }

    /// Cancelled when the feed closes (the session ended or changed).
    pub(crate) fn closed_signal(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// The feed's live records from now on (for a reader that takes its own
    /// snapshot right after subscribing).
    pub(crate) fn live(&self) -> broadcast::Receiver<FeedRecord> {
        self.live.subscribe()
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
        // A byte-bounded feed never keeps or delivers a record larger than
        // its per-record cap.
        let event = if state.ring.has_byte_budget() {
            let size = serde_json::to_vec(&event).map_or(usize::MAX, |bytes| bytes.len());
            if size > BOUNDED_RECORD_MAX {
                match event {
                    ServerEvent::PermissionAsk { ask_id, .. } => {
                        state.projection.oversized_asks.insert(ask_id);
                        ServerEvent::Warning {
                            message: "a permission request too large to share is open on the PC"
                                .to_string(),
                        }
                    }
                    ServerEvent::Plan { .. } => {
                        state.projection.oversized_plan = true;
                        state.projection.plan = None;
                        ServerEvent::Warning {
                            message: "the current plan is too large to share".to_string(),
                        }
                    }
                    // Streamed text arrives split (see `bounded_records`); a
                    // piece still over the cap leaves the text unknown rather
                    // than empty.
                    ServerEvent::TextDelta { .. } => {
                        state.projection.turn_text = None;
                        state.projection.turn_text_truncated = false;
                        ServerEvent::Warning {
                            message: "part of the answer is shown only on the PC".to_string(),
                        }
                    }
                    _ => ServerEvent::Warning {
                        message: "a large update is shown only on the PC".to_string(),
                    },
                }
            } else {
                event
            }
        } else {
            event
        };
        state.projection.apply(&event);
        if let Some(mirror) = &mut state.mirror {
            // A finished turn leaves nothing running or waiting that the
            // mirror knows of; the source's next snapshot says more.
            if matches!(event, ServerEvent::Stopped { .. }) {
                mirror.admission = Admission::default();
            }
        }
        let size = if state.ring.has_byte_budget() {
            serde_json::to_vec(&event).map_or(0, |bytes| bytes.len())
        } else {
            0
        };
        let seq = state.ring.push_sized(event.clone(), size);
        let _ = self.live.send((seq, event));
    }

    /// The pump lost source events: start a new epoch whose projection holds
    /// only what is still authoritative.
    fn rebuild_asks(&self, outstanding: Vec<localpilot_rpc::PendingAsk>, lost_runtime: bool) {
        let mut state = self.state();
        if state.closed.is_some() {
            return;
        }
        // One policy for the seed and every rebuild: in a byte-bounded feed an
        // ask too large to keep is recorded as oversized, never kept, and the
        // oversized set is recomputed from what is pending now.
        let bounded = state.ring.has_byte_budget();
        state.projection.asks.clear();
        state.projection.oversized_asks.clear();
        for ask in &outstanding {
            if bounded && encoded_len(&ask.wire_event()) > BOUNDED_RECORD_MAX {
                state.projection.oversized_asks.insert(ask.ask_id.clone());
            } else {
                state
                    .projection
                    .asks
                    .insert(ask.ask_id.clone(), ask.detail());
            }
        }
        if lost_runtime {
            state.losses += 1;
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

    /// A source feed's snapshot at its head (history and admission are the
    /// host's, published to the feed), with how many losses the feed had when
    /// it was taken; or the closure, when the feed has closed.
    pub(crate) fn source_snapshot(&self) -> (ServerEvent, u64) {
        let state = self.state();
        let event = match &state.closed {
            Some(reason) => ServerEvent::SessionClosed {
                reason: reason.clone(),
            },
            None => snapshot(&state, &(Vec::new(), true), Admission::default()),
        };
        (event, state.losses)
    }

    /// Changes to a source feed's published history or admission: a reader
    /// that shares the feed sends a fresh snapshot when this moves.
    pub(crate) fn source_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.history.subscribe()
    }

    /// Publish the host's history tail (a source feed), in place: the
    /// projection and position are unchanged.
    fn set_history(&self, history: (Vec<HistoryMessage>, bool)) {
        {
            let mut state = self.state();
            if state.closed.is_some() {
                return;
            }
            let admission = state
                .source
                .as_ref()
                .map(|s| s.admission)
                .unwrap_or_default();
            state.source = Some(MirrorExtra { history, admission });
        }
        self.history.send_modify(|n| *n += 1);
    }

    /// Publish the host's admission state (a source feed): whether accepted
    /// work is running or about to start and how many follow-ups wait.
    #[cfg_attr(not(feature = "tui"), allow(dead_code))]
    pub(crate) fn set_admission(&self, admission: Admission) {
        {
            let mut state = self.state();
            if state.closed.is_some() {
                return;
            }
            let history = state
                .source
                .take()
                .map_or_else(|| (Vec::new(), true), |s| s.history);
            state.source = Some(MirrorExtra { history, admission });
        }
        self.history.send_modify(|n| *n += 1);
    }

    /// The host replaced what the session shows (its conversation was cleared
    /// or compacted) while nothing runs: a new epoch whose projection is the
    /// known idle state (the outstanding asks rebuilt from the registry) and
    /// whose history is `history`, all under one lock, and only then a
    /// `resync_required`. Not a loss: [`SessionFeed::source_snapshot`]'s loss
    /// count is unchanged.
    fn refresh(
        &self,
        history: (Vec<HistoryMessage>, bool),
        outstanding: Vec<localpilot_rpc::PendingAsk>,
    ) {
        {
            let mut state = self.state();
            if state.closed.is_some() {
                return;
            }
            let bounded = state.ring.has_byte_budget();
            state.epoch = new_epoch();
            state.ring.clear();
            state.projection = Projection::new();
            for ask in &outstanding {
                if bounded && encoded_len(&ask.wire_event()) > BOUNDED_RECORD_MAX {
                    state.projection.oversized_asks.insert(ask.ask_id.clone());
                } else {
                    state
                        .projection
                        .asks
                        .insert(ask.ask_id.clone(), ask.detail());
                }
            }
            let admission = state
                .source
                .as_ref()
                .map(|s| s.admission)
                .unwrap_or_default();
            state.source = Some(MirrorExtra { history, admission });
            let event = ServerEvent::ResyncRequired {
                epoch: state.epoch.clone(),
                seq: state.ring.head() + 1,
            };
            let seq = state.ring.push(event.clone());
            let _ = self.live.send((seq, event));
        }
        self.history.send_modify(|n| *n += 1);
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

fn encoded_len(event: &ServerEvent) -> usize {
    serde_json::to_vec(event).map_or(usize::MAX, |bytes| bytes.len())
}

/// Text per streamed piece when a large text record is split for a
/// byte-bounded feed: escaping expands a byte to at most six, so a piece stays
/// well under [`BOUNDED_RECORD_MAX`].
const SPLIT_TEXT: usize = 4 * 1024;

fn split_text(text: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let mut cut = rest.len().min(SPLIT_TEXT);
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        let (piece, tail) = rest.split_at(cut);
        pieces.push(piece.to_string());
        rest = tail;
    }
    pieces
}

/// The records a byte-bounded feed takes for one runtime event: large text
/// split into pieces (so the projection keeps its whole bounded tail), and
/// everything else as one record (sized again when pushed).
pub(crate) fn bounded_records(event: RuntimeEvent) -> Vec<ServerEvent> {
    let mapped = match event {
        RuntimeEvent::TurnStarted => Some(ServerEvent::TurnStarted),
        event => map_event(event),
    };
    match mapped {
        Some(ServerEvent::TextDelta { text }) if text.len() > SPLIT_TEXT => split_text(&text)
            .into_iter()
            .map(|text| ServerEvent::TextDelta { text })
            .collect(),
        Some(ServerEvent::ReasoningDelta { text }) if text.len() > SPLIT_TEXT => split_text(&text)
            .into_iter()
            .map(|text| ServerEvent::ReasoningDelta { text })
            .collect(),
        Some(event) => vec![event],
        None => Vec::new(),
    }
}

/// Records waiting between a source's per-turn forwarders and its feed's pump.
/// Bounded in count and bytes; when a record does not fit, everything queued is
/// dropped and a loss is latched, and the pump rebuilds before it applies
/// anything queued afterwards. A producer never waits.
#[derive(Default)]
pub(crate) struct Ingress {
    state: Mutex<IngressState>,
    wake: tokio::sync::Notify,
}

#[derive(Default)]
struct IngressState {
    queue: std::collections::VecDeque<Item>,
    bytes: usize,
    lost: bool,
    /// The newest history dropped with a loss: applied after the rebuild (the
    /// projection stays unknown; only the history is current). One tail at
    /// most, as bounded as any snapshot's.
    carried: Option<(Vec<HistoryMessage>, bool)>,
}

/// The bytes a history tail holds, as an ingress charges it.
fn history_size(history: &(Vec<HistoryMessage>, bool)) -> usize {
    history
        .0
        .iter()
        .map(|message| message.text.len() + message.role.len() + 32)
        .sum::<usize>()
        + 32
}

impl Item {
    fn size(&self) -> usize {
        match self {
            Self::Record(record) => encoded_len(record),
            Self::History(history) | Self::Refresh(history) => history_size(history),
        }
    }
}

/// One entry in an ingress, in the order the host produced it.
enum Item {
    Record(ServerEvent),
    /// The host's history tail after an operation.
    History((Vec<HistoryMessage>, bool)),
    /// The host replaced what the session shows; see [`SessionFeed::refresh`].
    Refresh((Vec<HistoryMessage>, bool)),
}

impl IngressState {
    /// Drop everything queued and latch a loss, keeping only the newest
    /// history among what was dropped.
    fn latch_loss(&mut self) {
        for item in self.queue.drain(..) {
            if let Item::History(history) | Item::Refresh(history) = item {
                self.carried = Some(history);
            }
        }
        self.bytes = 0;
        self.lost = true;
    }
}

/// Records an ingress holds at most.
const INGRESS_RECORDS: usize = 256;
/// Bytes an ingress holds at most.
const INGRESS_BYTES: usize = 4 * 1024 * 1024;

/// What the pump takes next from an ingress.
enum Taken {
    Record(ServerEvent),
    History((Vec<HistoryMessage>, bool)),
    Refresh((Vec<HistoryMessage>, bool)),
    Lost(Option<(Vec<HistoryMessage>, bool)>),
}

impl Ingress {
    fn lock(&self) -> MutexGuard<'_, IngressState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queue records for the feed, or latch a loss when they do not fit.
    pub(crate) fn offer(&self, records: Vec<ServerEvent>) {
        {
            let mut state = self.lock();
            for record in records {
                if state.lost {
                    break; // nothing more until the pump has rebuilt
                }
                let size = encoded_len(&record);
                if state.queue.len() >= INGRESS_RECORDS
                    || state.bytes.saturating_add(size) > INGRESS_BYTES
                {
                    state.latch_loss();
                } else {
                    state.bytes += size;
                    state.queue.push_back(Item::Record(record));
                }
            }
        }
        self.wake.notify_one();
    }

    /// Latch a loss (a forwarder fell behind its turn's channel).
    pub(crate) fn lost(&self) {
        self.lock().latch_loss();
        self.wake.notify_one();
    }

    /// Queue the host's history tail (after the records already queued). A
    /// history waiting directly before it is superseded.
    pub(crate) fn history(&self, history: (Vec<HistoryMessage>, bool)) {
        self.control(Item::History(history));
    }

    /// Queue a refresh: the host replaced what the session shows (after the
    /// records already queued).
    pub(crate) fn refresh(&self, history: (Vec<HistoryMessage>, bool)) {
        self.control(Item::Refresh(history));
    }

    /// Queue a history or refresh after everything queued, under the same
    /// count and byte bounds as records. Consecutive ones merge into one (the
    /// newest history; a refresh if either was). One that does not fit latches
    /// a loss, keeping only it as the newest history.
    fn control(&self, item: Item) {
        {
            let mut state = self.lock();
            let mut item = item;
            if matches!(
                state.queue.back(),
                Some(Item::History(_) | Item::Refresh(_))
            ) {
                if let Some(previous) = state.queue.pop_back() {
                    state.bytes = state.bytes.saturating_sub(previous.size());
                    let refresh =
                        matches!(previous, Item::Refresh(_)) || matches!(item, Item::Refresh(_));
                    if let Item::History(history) | Item::Refresh(history) = item {
                        item = if refresh {
                            Item::Refresh(history)
                        } else {
                            Item::History(history)
                        };
                    }
                }
            }
            let size = item.size();
            if !state.lost
                && (state.queue.len() >= INGRESS_RECORDS
                    || state.bytes.saturating_add(size) > INGRESS_BYTES)
            {
                state.latch_loss();
            }
            if state.lost {
                // Nothing queued survives the rebuild; the newest history does.
                if let Item::History(history) | Item::Refresh(history) = item {
                    state.carried = Some(history);
                }
            } else {
                state.bytes += size;
                state.queue.push_back(item);
            }
        }
        self.wake.notify_one();
    }

    /// A latched loss comes first, before anything queued after it.
    fn take(&self) -> Option<Taken> {
        let mut state = self.lock();
        if state.lost {
            state.lost = false;
            state.queue.clear();
            state.bytes = 0;
            return Some(Taken::Lost(state.carried.take()));
        }
        let item = state.queue.pop_front()?;
        state.bytes = state.bytes.saturating_sub(item.size());
        Some(match item {
            Item::Record(record) => Taken::Record(record),
            Item::History(history) => Taken::History(history),
            Item::Refresh(history) => Taken::Refresh(history),
        })
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
    // A mirror's history and admission are its source's, and a source feed's
    // are its host's, each held with the projection; the caller's are for
    // hosted sessions only.
    let (history, admission) = match state.mirror.as_ref().or(state.source.as_ref()) {
        Some(extra) => (&extra.history, extra.admission),
        None => (history, admission),
    };
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
/// The pump of a byte-bounded feed fed through an [`Ingress`].
async fn pump_ingress(
    feed: Arc<SessionFeed>,
    ingress: Arc<Ingress>,
    asks: Option<AskRegistry>,
    mut ask_rx: Option<broadcast::Receiver<AskEvent>>,
) {
    loop {
        // Subscribe to the wake-up before draining, so nothing offered in
        // between is missed.
        let woken = ingress.wake.notified();
        while let Some(taken) = ingress.take() {
            match taken {
                Taken::Record(record) => feed.push(record),
                Taken::History(history) => feed.set_history(history),
                Taken::Refresh(history) => {
                    // The registry is the authority for what is outstanding
                    // now; drop ask events received before it was read.
                    if let Some(rx) = &mut ask_rx {
                        *rx = rx.resubscribe();
                    }
                    let outstanding = asks
                        .as_ref()
                        .map(AskRegistry::outstanding_detail)
                        .unwrap_or_default();
                    feed.refresh(history, outstanding);
                }
                Taken::Lost(carried) => {
                    if let Some(rx) = &mut ask_rx {
                        *rx = rx.resubscribe();
                    }
                    rebuild(&feed, asks.as_ref());
                    if let Some(history) = carried {
                        feed.set_history(history);
                    }
                }
            }
        }
        tokio::select! {
            biased;
            () = feed.cancel.cancelled() => break,
            () = woken => {}
            event = next_ask(&mut ask_rx) => match event {
                Ok(AskEvent::Raised(ask)) => {
                    let known = {
                        let state = feed.state();
                        state.projection.asks.contains_key(&ask.ask_id)
                            || state.projection.oversized_asks.contains(&ask.ask_id)
                    };
                    if !known {
                        feed.push(ask.wire_event());
                    }
                }
                Ok(AskEvent::Resolved { ask_id, outcome, by }) => {
                    feed.push(ServerEvent::PermissionResolved { ask_id, outcome, by });
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if let Some(rx) = &mut ask_rx {
                        *rx = rx.resubscribe();
                    }
                    rebuild(&feed, asks.as_ref());
                }
                Err(broadcast::error::RecvError::Closed) => ask_rx = None,
            },
        }
    }
}

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
        let feed = Arc::new(SessionFeed::new(16, usize::MAX));
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
        let feed = Arc::new(SessionFeed::new(16, usize::MAX));
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
        let feed = Arc::new(SessionFeed::new(16, usize::MAX));
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
        let feed = Arc::new(SessionFeed::new(16, usize::MAX));
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
        let feed = Arc::new(SessionFeed::new(4, usize::MAX));
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
        let feed = Arc::new(SessionFeed::new(16, usize::MAX));
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

    // --- the bounded source ingress ---------------------------------------

    #[test]
    fn an_ingress_overflow_latches_a_loss_taken_before_anything_offered_later() {
        let ingress = Ingress::default();
        ingress.offer((0..INGRESS_RECORDS + 1).map(|_| text("a")).collect());
        // Offered while the loss is latched: dropped.
        ingress.offer(vec![text("late")]);
        assert!(matches!(ingress.take(), Some(Taken::Lost(_))));
        assert!(ingress.take().is_none(), "nothing from before the rebuild");
        ingress.offer(vec![text("after")]);
        assert!(
            matches!(ingress.take(), Some(Taken::Record(ServerEvent::TextDelta { text })) if text == "after")
        );
    }

    #[test]
    fn a_loss_latched_by_the_last_offer_is_still_taken() {
        let ingress = Ingress::default();
        ingress.offer((0..INGRESS_RECORDS + 1).map(|_| text("a")).collect());
        // No more records ever arrive: the pump still sees the loss.
        assert!(matches!(ingress.take(), Some(Taken::Lost(_))));
    }

    #[test]
    fn an_ingress_holds_a_bounded_number_of_bytes() {
        let ingress = Ingress::default();
        let mib = "m".repeat(1024 * 1024);
        ingress.offer((0..5).map(|_| text(&mib)).collect());
        assert!(matches!(ingress.take(), Some(Taken::Lost(_))));
        assert!(ingress.take().is_none());
    }

    #[tokio::test]
    async fn a_lost_ingress_makes_the_feed_claim_nothing_until_the_next_turn() {
        let ingress = Arc::new(Ingress::default());
        let feed = SessionFeed::start_fed(ingress.clone(), None, 8 * 1024 * 1024);
        ingress.offer(vec![ServerEvent::TurnStarted, text("partial")]);
        ingress.lost();
        let history = (Vec::new(), false);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let (snapshot, _) = feed.snapshot(&history, &Admission::default);
                if matches!(
                    snapshot,
                    ServerEvent::Snapshot {
                        turn: TurnState::Unknown,
                        turn_text: None,
                        ..
                    }
                ) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the loss reached the feed");
    }

    #[test]
    fn a_rebuild_recomputes_kept_and_oversized_asks_from_what_is_pending() {
        let feed = SessionFeed::new(16, 8 * 1024 * 1024);
        let ask = |id: &str, detail: usize| localpilot_rpc::PendingAsk {
            ask_id: id.to_string(),
            tool: "t".to_string(),
            detail: "d".repeat(detail),
            risk: "r".to_string(),
            expires_at_ms: 0,
        };
        feed.rebuild_asks(
            vec![ask("small", 10), ask("big", BOUNDED_RECORD_MAX + 1)],
            true,
        );
        assert!(!feed.shareable());
        let (asks, _) = feed.pending_asks();
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].ask_id, "small");
        // The big ask was resolved while records were lost: the next rebuild
        // forgets it.
        feed.rebuild_asks(vec![ask("small", 10)], true);
        assert!(feed.shareable());
    }

    fn shown(text: &str) -> (Vec<HistoryMessage>, bool) {
        (
            vec![HistoryMessage {
                role: "assistant".to_string(),
                text: text.to_string(),
            }],
            false,
        )
    }

    async fn settled(
        feed: &SessionFeed,
        mut done: impl FnMut(&ServerEvent, u64) -> bool,
    ) -> (ServerEvent, u64) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let (snapshot, losses) = feed.source_snapshot();
                if done(&snapshot, losses) {
                    return (snapshot, losses);
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the feed settled")
    }

    #[tokio::test]
    async fn a_refresh_starts_a_known_idle_epoch_and_is_not_a_loss() {
        let ingress = Arc::new(Ingress::default());
        let feed = SessionFeed::start_fed(ingress.clone(), None, 8 * 1024 * 1024);
        ingress.offer(vec![ServerEvent::TurnStarted, text("old answer")]);
        let (before, _) = settled(&feed, |s, _| {
            matches!(
                s,
                ServerEvent::Snapshot {
                    turn: TurnState::Running,
                    ..
                }
            )
        })
        .await;
        let ServerEvent::Snapshot {
            epoch: old_epoch, ..
        } = before
        else {
            unreachable!()
        };
        ingress.refresh(shown("after clear"));
        let (after, losses) = settled(&feed, |s, _| {
            matches!(s, ServerEvent::Snapshot { history, .. } if history.first().is_some_and(|m| m.text == "after clear"))
        })
        .await;
        let ServerEvent::Snapshot {
            epoch,
            turn,
            turn_text,
            plan,
            ..
        } = after
        else {
            unreachable!()
        };
        assert_ne!(epoch, old_epoch, "a new epoch");
        assert_eq!(turn, TurnState::Idle, "known idle, not unknown");
        assert_eq!(turn_text.as_deref(), Some(""), "no stale text");
        assert!(plan.is_none());
        assert_eq!(losses, 0, "a refresh is not a loss");
    }

    #[tokio::test]
    async fn a_loss_racing_a_refresh_still_counts_and_keeps_the_newest_history() {
        let ingress = Arc::new(Ingress::default());
        let feed = SessionFeed::start_fed(ingress.clone(), None, 8 * 1024 * 1024);
        // Both before the pump runs: the refresh is dropped with the loss.
        ingress.refresh(shown("newest"));
        ingress.lost();
        let (after, losses) = settled(&feed, |_, losses| losses > 0).await;
        assert_eq!(losses, 1);
        let ServerEvent::Snapshot { turn, history, .. } = after else {
            unreachable!()
        };
        assert_eq!(
            turn,
            TurnState::Unknown,
            "a dropped refresh never claims idle"
        );
        let (newest, _) = settled(&feed, |s, _| {
            matches!(s, ServerEvent::Snapshot { history, .. } if history.first().is_some_and(|m| m.text == "newest"))
        })
        .await;
        drop((history, newest));
    }

    #[tokio::test]
    async fn published_history_keeps_the_position_and_signals_a_change() {
        let ingress = Arc::new(Ingress::default());
        let feed = SessionFeed::start_fed(ingress.clone(), None, 8 * 1024 * 1024);
        let mut changes = feed.source_changes();
        let (before, _) = feed.source_snapshot();
        ingress.history(shown("one"));
        tokio::time::timeout(std::time::Duration::from_secs(5), changes.changed())
            .await
            .unwrap()
            .unwrap();
        let (after, losses) = feed.source_snapshot();
        let (
            ServerEvent::Snapshot {
                epoch: a, seq: x, ..
            },
            ServerEvent::Snapshot {
                epoch: b,
                seq: y,
                history,
                ..
            },
        ) = (before, after)
        else {
            unreachable!()
        };
        assert_eq!((a, x), (b, y), "same epoch and position");
        assert_eq!(history[0].text, "one");
        assert_eq!(losses, 0);
    }

    #[test]
    fn histories_and_records_share_the_ingress_bounds() {
        let ingress = Ingress::default();
        // Consecutive histories merge into one entry.
        for n in 0..1000 {
            ingress.history(shown(&format!("tail {n}")));
        }
        assert_eq!(ingress.lock().queue.len(), 1);
        // Records and histories in turn, with no pump draining them.
        let mut lost_at = None;
        for n in 0..400 {
            ingress.offer(vec![text("piece")]);
            ingress.history(shown(&format!("tail {n}")));
            let state = ingress.lock();
            assert!(state.queue.len() <= INGRESS_RECORDS, "count bound");
            assert!(state.bytes <= INGRESS_BYTES, "byte bound");
            if state.lost && lost_at.is_none() {
                lost_at = Some(n);
            }
        }
        assert!(lost_at.is_some(), "overflow latched a loss");
        // The loss comes first, carrying only the newest history.
        match ingress.take() {
            Some(Taken::Lost(Some((history, _)))) => assert_eq!(history[0].text, "tail 399"),
            _ => panic!("expected the loss with the newest history"),
        }
        assert!(ingress.take().is_none(), "nothing else retained");
        assert_eq!(ingress.lock().bytes, 0);
    }

    #[test]
    fn a_refresh_merged_with_a_later_history_stays_a_refresh() {
        let ingress = Ingress::default();
        ingress.refresh(shown("cleared"));
        ingress.history(shown("newer"));
        match ingress.take() {
            Some(Taken::Refresh((history, _))) => assert_eq!(history[0].text, "newer"),
            _ => panic!("expected one refresh"),
        }
        assert!(ingress.take().is_none());
        assert_eq!(ingress.lock().bytes, 0, "its bytes were released");
    }
}
