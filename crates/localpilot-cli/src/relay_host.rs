//! The source's final say over commands paired devices send to a shared
//! session.
//!
//! The server admits a device's command first (standing, scope, retry key)
//! and forwards it to the session's source as a numbered `relay_command`. The
//! source decides it again: the share and relay it names must be the live
//! ones, the device must still be allowed and hold the scope, the request must
//! be new, and a remote command must be one a device may run. Permission
//! answers go straight to the session's one ask registry (first answer wins).
//! Prompts, cancels and commands go to the session's host as a
//! [`SourceRequest`]: the request stays *pending* (nothing has happened) until
//! the host decides it, immediately before acting, with
//! [`Ticket::accept_now`] (the share still live, the device's current scope
//! still sufficient). A prompt the host queues is an accepted follow-up: it
//! survives the share stopping, and only the device's current authority is
//! checked again ([`Ticket::may_start`]) right before its turn starts.
//!
//! Everything kept per relay is bounded: at most [`DEVICES_MAX`] device
//! streams (a further device is refused before anything runs, so no existing
//! stream's duplicate fence is ever forgotten), [`WINDOW`] terminal results
//! per device plus its high-water mark, [`OPEN_MAX`] open requests, command
//! payloads of [`PAYLOAD_MAX`], result texts of [`RESULT_TEXT`], and one
//! coalesced latest state per open request on the way back to the server.
//!
//! Not yet used outside tests: the full-screen `/remote share` command serves
//! these requests in the change that adds the terminal's sharing commands.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use localpilot_rpc::{
    AskRegistry, ClientCommand, Principal, RelayRequestState, ReplyRefusal, Requirement, Scope,
};
use tokio::sync::{mpsc, Notify};

/// Terminal results remembered per device.
pub(crate) const WINDOW: usize = 32;
/// Requests not yet terminal (pending or queued), per relay.
pub(crate) const OPEN_MAX: usize = 64;
/// Device streams one relay keeps.
pub(crate) const DEVICES_MAX: usize = 64;
/// Bytes the request table may hold, metadata included.
pub(crate) const TABLE_BYTES: usize = 1024 * 1024;
/// A command's encoded size at most.
pub(crate) const PAYLOAD_MAX: usize = 64 * 1024;
/// A result or refusal text at most, in bytes, ellipsis included.
pub(crate) const RESULT_TEXT: usize = 4 * 1024;
/// What a device stream costs before any request: its id and high-water mark.
const STREAM_BYTES: usize = 64;
/// What a request entry costs besides its text.
const ENTRY_BYTES: usize = 48;

/// Cut a text to [`RESULT_TEXT`] bytes, marking the cut.
pub(crate) fn bounded_text(text: String) -> String {
    if text.len() <= RESULT_TEXT {
        return text;
    }
    let mark = "…";
    let mut cut = RESULT_TEXT - mark.len();
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{mark}", &text[..cut])
}

/// A state with its texts bounded.
fn bounded_state(state: RelayRequestState) -> RelayRequestState {
    match state {
        RelayRequestState::Rejected { reason } => RelayRequestState::Rejected {
            reason: bounded_text(reason),
        },
        RelayRequestState::Done {
            output: Some(output),
        } => RelayRequestState::Done {
            output: Some(bounded_text(output)),
        },
        state => state,
    }
}

/// What the source knows of a device's request number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Seen {
    /// Never seen: decide it now.
    New,
    /// Seen and not yet decided by the host.
    Pending,
    /// Seen and still remembered: its state again.
    Known(RelayRequestState),
    /// Seen and forgotten: it never runs now.
    Forgotten,
}

/// One request: `None` while pending.
type Entry = (u64, Option<RelayRequestState>);

#[derive(Default)]
struct DeviceWindow {
    highest: u64,
    entries: VecDeque<Entry>,
}

fn entry_bytes(state: Option<&RelayRequestState>) -> usize {
    ENTRY_BYTES
        + match state {
            Some(RelayRequestState::Rejected { reason }) => reason.len(),
            Some(RelayRequestState::Done { output }) => output.as_ref().map_or(0, String::len),
            _ => 0,
        }
}

fn is_open(state: Option<&RelayRequestState>) -> bool {
    state.map_or(true, |state| !state.is_terminal())
}

/// The bounded table of one relay's requests.
#[derive(Default)]
pub(crate) struct SourceWindow {
    devices: HashMap<String, DeviceWindow>,
    open: usize,
    bytes: usize,
}

impl SourceWindow {
    /// What the source knows of request `n` from `device`.
    pub(crate) fn seen(&self, device: &str, n: u64) -> Seen {
        let Some(window) = self.devices.get(device) else {
            return Seen::New;
        };
        match window.entries.iter().find(|(seen, _)| *seen == n) {
            Some((_, Some(state))) => Seen::Known(state.clone()),
            Some((_, None)) => Seen::Pending,
            None if n <= window.highest => Seen::Forgotten,
            None => Seen::New,
        }
    }

    /// Whether a request from `device` carrying `payload` bytes may be opened
    /// now, with `reserved` bytes already held by requests the host keeps: a
    /// place among the open requests, room in the budget for its entry and
    /// payload, and a stream (an existing one, or room for a new one).
    pub(crate) fn has_room(&self, device: &str, payload: usize, reserved: usize) -> bool {
        let stream_cost = if self.devices.contains_key(device) {
            0
        } else if self.devices.len() < DEVICES_MAX {
            STREAM_BYTES + device.len()
        } else {
            return false;
        };
        self.open < OPEN_MAX
            && self.bytes + reserved + stream_cost + ENTRY_BYTES + RESULT_TEXT + payload
                <= TABLE_BYTES
    }

    /// Record request `n` from `device` (`None`: pending).
    pub(crate) fn record(&mut self, device: &str, n: u64, state: Option<RelayRequestState>) {
        let state = state.map(bounded_state);
        if !self.devices.contains_key(device) {
            self.bytes += STREAM_BYTES + device.len();
            self.devices
                .insert(device.to_string(), DeviceWindow::default());
        }
        let Some(window) = self.devices.get_mut(device) else {
            return;
        };
        window.highest = window.highest.max(n);
        if let Some(slot) = window.entries.iter_mut().find(|(seen, _)| *seen == n) {
            if !is_open(slot.1.as_ref()) {
                return; // a terminal request never changes again
            }
            if !is_open(state.as_ref()) {
                self.open = self.open.saturating_sub(1);
            }
            self.bytes = self.bytes - entry_bytes(slot.1.as_ref()) + entry_bytes(state.as_ref());
            slot.1 = state;
        } else {
            if is_open(state.as_ref()) {
                self.open += 1;
            }
            self.bytes += entry_bytes(state.as_ref());
            window.entries.push_back((n, state));
        }
        self.evict();
    }

    /// Forget the oldest terminal entries beyond each device's window and the
    /// byte budget. Open requests, streams and high-water marks are never
    /// forgotten.
    fn evict(&mut self) {
        for window in self.devices.values_mut() {
            while window
                .entries
                .iter()
                .filter(|(_, state)| !is_open(state.as_ref()))
                .count()
                > WINDOW
            {
                let Some(index) = window
                    .entries
                    .iter()
                    .position(|(_, state)| !is_open(state.as_ref()))
                else {
                    break;
                };
                if let Some((_, state)) = window.entries.remove(index) {
                    self.bytes -= entry_bytes(state.as_ref());
                }
            }
        }
        while self.bytes > TABLE_BYTES {
            let oldest = self.devices.iter().find_map(|(device, window)| {
                window
                    .entries
                    .iter()
                    .position(|(_, state)| !is_open(state.as_ref()))
                    .map(|index| (device.clone(), index))
            });
            let Some((device, index)) = oldest else {
                break;
            };
            if let Some(window) = self.devices.get_mut(&device) {
                if let Some((_, state)) = window.entries.remove(index) {
                    self.bytes -= entry_bytes(state.as_ref());
                }
            }
        }
    }

    /// The bytes the table accounts for (streams and entries).
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    /// The device streams kept.
    pub(crate) fn streams(&self) -> usize {
        self.devices.len()
    }
}

/// The states hosts reported, on their way back to the server: the latest
/// state per request, coalesced, so the table never grows past the requests
/// that exist and a stalled consumer never blocks a host or loses the final
/// state.
#[derive(Default)]
pub(crate) struct Updates {
    latest: Mutex<BTreeMap<(String, u64), RelayRequestState>>,
    wake: Notify,
}

impl Updates {
    fn report(&self, device: &str, n: u64, state: RelayRequestState) {
        self.latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((device.to_string(), n), bounded_state(state));
        self.wake.notify_one();
    }

    /// Wait until something was reported, then take every latest state.
    pub(crate) async fn next(&self) -> Vec<(String, u64, RelayRequestState)> {
        loop {
            let woken = self.wake.notified();
            let taken: Vec<_> =
                std::mem::take(&mut *self.latest.lock().unwrap_or_else(PoisonError::into_inner))
                    .into_iter()
                    .map(|((device, n), state)| (device, n, state))
                    .collect();
            if !taken.is_empty() {
                return taken;
            }
            woken.await;
        }
    }

    /// How many requests have a state waiting.
    pub(crate) fn waiting(&self) -> usize {
        self.latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// A device's current scope, or `None` when it is no longer allowed here.
pub(crate) type Standing = Arc<dyn Fn(&str) -> Option<Scope> + Send + Sync>;

/// The payload bytes a request the host holds keeps reserved in its relay's
/// budget, given back when the host lets the request go.
struct Reservation {
    bytes: usize,
    pool: Arc<AtomicUsize>,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.pool.fetch_sub(self.bytes, Ordering::SeqCst);
    }
}

/// How the host decides and reports one request. Holding it keeps the
/// request's payload reserved in the relay's budget; dropping it (the request
/// ran, was refused, or was let go) gives the bytes back.
pub(crate) struct Ticket {
    device: String,
    n: u64,
    required: Scope,
    updates: Arc<Updates>,
    standing: Standing,
    live: Arc<AtomicBool>,
    _reservation: Reservation,
}

impl Ticket {
    /// Report a transition (queued, accepted, rejected, done).
    pub(crate) fn report(&self, state: RelayRequestState) {
        self.updates.report(&self.device, self.n, state);
    }

    /// Whether the request may be accepted now, immediately before the host
    /// acts on it or queues it: the share is still live and the device's
    /// current scope still allows it.
    pub(crate) fn accept_now(&self) -> bool {
        self.live.load(Ordering::SeqCst) && self.may_start()
    }

    /// Whether an accepted, queued follow-up may start now: the device's
    /// current scope still allows it. It does not depend on the share still
    /// being live (an accepted follow-up survives the share stopping).
    pub(crate) fn may_start(&self) -> bool {
        (self.standing)(&self.device).is_some_and(|scope| scope >= self.required)
    }
}

/// One command for the session's host to decide and run.
pub(crate) struct SourceRequest {
    pub(crate) command: ClientCommand,
    pub(crate) ticket: Ticket,
}

/// What a relay needs to take commands for a session.
#[derive(Clone)]
pub(crate) struct SourceHost {
    /// Where requests go (the session's host services this).
    pub(crate) requests: mpsc::Sender<SourceRequest>,
    pub(crate) standing: Standing,
    /// The session's one ask registry.
    pub(crate) asks: AskRegistry,
}

/// Whether `name` is a slash command a device may run on a shared session.
pub(crate) fn remote_command(name: &str) -> bool {
    localpilot_slash::lookup(name)
        .is_some_and(|spelling| localpilot_slash::remote_description(spelling.command).is_some())
}

/// One relay's admission of forwarded commands.
pub(crate) struct SourceAdmission {
    host: Option<SourceHost>,
    window: SourceWindow,
    live: Arc<AtomicBool>,
    updates: Arc<Updates>,
    /// Payload bytes held by requests the host still has.
    reserved: Arc<AtomicUsize>,
}

impl SourceAdmission {
    /// Admission for one relay. Without a host, every command is refused.
    pub(crate) fn new(host: Option<SourceHost>) -> Self {
        Self {
            host,
            window: SourceWindow::default(),
            live: Arc::new(AtomicBool::new(true)),
            updates: Arc::new(Updates::default()),
            reserved: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Payload bytes held by requests the host still has.
    pub(crate) fn reserved(&self) -> usize {
        self.reserved.load(Ordering::SeqCst)
    }

    /// The states hosts report, for the relay to send.
    pub(crate) fn updates(&self) -> Arc<Updates> {
        self.updates.clone()
    }

    /// Sharing ended: requests the host has not yet accepted are refused when
    /// it decides them. Accepted follow-ups are the host's own work now.
    pub(crate) fn end(&self) {
        self.live.store(false, Ordering::SeqCst);
    }

    /// Record a state the host reported.
    pub(crate) fn note(&mut self, device: &str, n: u64, state: RelayRequestState) {
        self.window.record(device, n, Some(state));
    }

    /// Decide command `n` from `device`. Returns the state to report now, or
    /// `None` when it is pending (the host reports later).
    pub(crate) fn decide(
        &mut self,
        device: &str,
        n: u64,
        command: ClientCommand,
    ) -> Option<RelayRequestState> {
        match self.window.seen(device, n) {
            Seen::Known(state) => return Some(state),
            Seen::Pending => return None,
            Seen::Forgotten => return Some(RelayRequestState::Unknown),
            Seen::New => {}
        }
        // A request that cannot get a place (or whose payload would not fit
        // the budget) is refused before anything runs, and not recorded: no
        // existing fence is forgotten for it.
        let payload = serde_json::to_vec(&command).map_or(usize::MAX, |bytes| bytes.len());
        if payload > PAYLOAD_MAX {
            return Some(RelayRequestState::Rejected {
                reason: "the request is too large".to_string(),
            });
        }
        if !self.window.has_room(device, payload, self.reserved()) {
            return Some(RelayRequestState::Rejected {
                reason: "too many requests are waiting".to_string(),
            });
        }
        let state = self.admit(device, n, command);
        self.window.record(device, n, state.clone());
        state
    }

    fn admit(&mut self, device: &str, n: u64, command: ClientCommand) -> Option<RelayRequestState> {
        let refuse = |reason: &str| {
            Some(RelayRequestState::Rejected {
                reason: reason.to_string(),
            })
        };
        let Some(host) = &self.host else {
            return refuse("this session does not take commands");
        };
        if !self.live.load(Ordering::SeqCst) {
            return refuse("sharing stopped");
        }
        let Some(scope) = (host.standing)(device) else {
            return refuse("this device is no longer allowed");
        };
        let required = match command.requirement() {
            Requirement::AtLeast(needed) if scope >= needed => needed,
            _ => return refuse("this device may not do that"),
        };
        let payload = serde_json::to_vec(&command).map_or(usize::MAX, |bytes| bytes.len());
        match command {
            ClientCommand::PermissionReply { ask_id, allow } => match host.asks.reply(
                &ask_id,
                allow,
                Principal::Device {
                    id: device.to_string(),
                },
            ) {
                Ok(_) => Some(RelayRequestState::Done { output: None }),
                Err(ReplyRefusal::AlreadyResolved(_)) => refuse("already answered"),
                Err(ReplyRefusal::Expired) => refuse("the request expired"),
                Err(ReplyRefusal::Unknown) => refuse("no such request"),
            },
            ClientCommand::RunCommand { ref name, .. } if !remote_command(name) => {
                refuse("that command is not available on a shared session")
            }
            command @ (ClientCommand::Prompt { .. }
            | ClientCommand::Cancel
            | ClientCommand::RunCommand { .. }) => {
                self.reserved.fetch_add(payload, Ordering::SeqCst);
                let ticket = Ticket {
                    device: device.to_string(),
                    n,
                    required,
                    updates: self.updates.clone(),
                    standing: host.standing.clone(),
                    live: self.live.clone(),
                    _reservation: Reservation {
                        bytes: payload,
                        pool: self.reserved.clone(),
                    },
                };
                match host.requests.try_send(SourceRequest { command, ticket }) {
                    Ok(()) => None,
                    Err(_) => refuse("the session is busy"),
                }
            }
            _ => refuse("not available on a shared session"),
        }
    }
}

impl Drop for SourceAdmission {
    fn drop(&mut self) {
        self.end();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU8;

    use super::*;

    fn done() -> RelayRequestState {
        RelayRequestState::Done { output: None }
    }

    #[test]
    fn a_repeat_gets_its_recorded_state_and_a_forgotten_request_never_runs() {
        let mut window = SourceWindow::default();
        window.record("phone", 1, Some(done()));
        assert_eq!(window.seen("phone", 1), Seen::Known(done()));
        for n in 2..=(WINDOW as u64 + 1) {
            window.record("phone", n, Some(done()));
        }
        assert_eq!(window.seen("phone", 1), Seen::Forgotten);
        assert_eq!(window.seen("phone", WINDOW as u64 + 2), Seen::New);
        assert_eq!(window.seen("tablet", 1), Seen::New, "per device");
    }

    #[test]
    fn open_requests_are_bounded_and_never_evicted() {
        let mut window = SourceWindow::default();
        for n in 1..=OPEN_MAX as u64 {
            window.record("phone", n, None);
        }
        assert!(!window.has_room("phone", 0, 0));
        for n in 1000..1100 {
            window.record("tablet", n, Some(done()));
        }
        assert_eq!(window.seen("phone", 1), Seen::Pending);
        window.record("phone", 1, Some(done()));
        assert!(
            window.has_room("phone", 0, 0),
            "a finished request frees its place"
        );
    }

    #[test]
    fn device_streams_are_capped_and_every_byte_is_accounted() {
        let mut window = SourceWindow::default();
        let long = "r".repeat(10 * RESULT_TEXT);
        for device in 0..DEVICES_MAX {
            let id = format!("device-{device}");
            assert!(window.has_room(&id, 0, 0));
            window.record(
                &id,
                1,
                Some(RelayRequestState::Rejected {
                    reason: long.clone(),
                }),
            );
        }
        // Every stream stays (its fence), and a new one is refused rather than
        // any old one forgotten.
        assert_eq!(window.streams(), DEVICES_MAX);
        assert!(!window.has_room("one-more", 0, 0));
        assert!(
            window.has_room("device-0", 0, 0),
            "an existing stream still may"
        );
        assert!(window.bytes() <= TABLE_BYTES);
        // Texts are bounded on the way in, ellipsis included.
        let Seen::Known(RelayRequestState::Rejected { reason }) = window.seen("device-0", 1) else {
            panic!("a recorded rejection")
        };
        assert!(reason.len() <= RESULT_TEXT);
    }

    #[test]
    fn a_terminal_request_never_reopens() {
        let mut window = SourceWindow::default();
        window.record("phone", 1, None);
        window.record("phone", 1, Some(done()));
        window.record("phone", 1, Some(RelayRequestState::Queued));
        assert_eq!(window.seen("phone", 1), Seen::Known(done()));
    }

    struct Fixture {
        scope: Arc<AtomicU8>,
        host: SourceHost,
        incoming: mpsc::Receiver<SourceRequest>,
    }

    /// 0 = not allowed, 1 = observe, 2 = control.
    fn fixture(scope: u8) -> Fixture {
        let scope = Arc::new(AtomicU8::new(scope));
        let (requests, incoming) = mpsc::channel(8);
        let reading = scope.clone();
        Fixture {
            host: SourceHost {
                requests,
                standing: Arc::new(move |_| match reading.load(Ordering::SeqCst) {
                    1 => Some(Scope::Observe),
                    2 => Some(Scope::Control),
                    _ => None,
                }),
                asks: AskRegistry::default(),
            },
            scope,
            incoming,
        }
    }

    fn prompt() -> ClientCommand {
        ClientCommand::Prompt {
            text: "hi".to_string(),
            disposition: Default::default(),
        }
    }

    #[test]
    fn a_pending_request_is_accepted_only_while_the_share_is_live_and_the_scope_holds() {
        let mut f = fixture(2);
        let mut admission = SourceAdmission::new(Some(f.host.clone()));
        assert_eq!(admission.decide("phone", 1, prompt()), None, "pending");
        assert_eq!(
            admission.decide("phone", 1, prompt()),
            None,
            "still pending"
        );
        let request = f.incoming.try_recv().unwrap();
        assert!(request.ticket.accept_now());
        // Downgraded while pending: refused before anything runs.
        f.scope.store(1, Ordering::SeqCst);
        assert!(!request.ticket.accept_now());
        // Restored, but the share stopped: still refused (not yet accepted).
        f.scope.store(2, Ordering::SeqCst);
        admission.end();
        assert!(!request.ticket.accept_now());
    }

    #[test]
    fn an_accepted_follow_up_survives_the_share_stopping_but_not_a_revocation() {
        let mut f = fixture(2);
        let mut admission = SourceAdmission::new(Some(f.host.clone()));
        admission.decide("phone", 1, prompt());
        let request = f.incoming.try_recv().unwrap();
        assert!(request.ticket.accept_now());
        request.ticket.report(RelayRequestState::Queued);
        drop(admission); // sharing stopped
        assert!(
            request.ticket.may_start(),
            "accepted work is the host's now"
        );
        f.scope.store(1, Ordering::SeqCst); // downgraded to observe
        assert!(!request.ticket.may_start(), "a downgrade stops it starting");
        f.scope.store(0, Ordering::SeqCst); // revoked
        assert!(!request.ticket.may_start());
    }

    #[test]
    fn the_source_refuses_what_the_device_may_no_longer_do() {
        let gone = fixture(0);
        let mut admission = SourceAdmission::new(Some(gone.host.clone()));
        assert!(matches!(
            admission.decide("phone", 1, prompt()),
            Some(RelayRequestState::Rejected { .. })
        ));
        let observer = fixture(1);
        let mut admission = SourceAdmission::new(Some(observer.host.clone()));
        assert!(matches!(
            admission.decide("tablet", 1, prompt()),
            Some(RelayRequestState::Rejected { .. })
        ));
        // A slash command a device may not run never reaches the host.
        let mut control = fixture(2);
        let mut admission = SourceAdmission::new(Some(control.host.clone()));
        let command = ClientCommand::RunCommand {
            name: "model".to_string(),
            args: "other".to_string(),
        };
        assert!(matches!(
            admission.decide("phone", 1, command),
            Some(RelayRequestState::Rejected { .. })
        ));
        assert!(control.incoming.try_recv().is_err());
    }

    #[test]
    fn a_device_answer_and_a_keyboard_answer_race_on_one_registry() {
        let control = fixture(2);
        let asks = control.host.asks.clone();
        let _open = asks.open_unbounded(localpilot_rpc::PendingAsk {
            ask_id: "a1".to_string(),
            tool: "t".to_string(),
            detail: "d".to_string(),
            risk: "r".to_string(),
            expires_at_ms: 0,
        });
        assert!(asks.resolve("a1", false), "the keyboard wins first");
        let mut admission = SourceAdmission::new(Some(control.host.clone()));
        let reply = ClientCommand::PermissionReply {
            ask_id: "a1".to_string(),
            allow: true,
        };
        let refused = Some(RelayRequestState::Rejected {
            reason: "already answered".to_string(),
        });
        assert_eq!(admission.decide("phone", 1, reply.clone()), refused);
        assert_eq!(admission.decide("phone", 1, reply), refused, "repeat: same");
    }

    #[tokio::test]
    async fn reported_states_coalesce_per_request_and_never_block_the_host() {
        let updates = Updates::default();
        for _ in 0..10_000 {
            updates.report("phone", 1, RelayRequestState::Queued);
        }
        updates.report("phone", 1, RelayRequestState::Accepted);
        updates.report("phone", 2, done());
        assert_eq!(updates.waiting(), 2, "one latest state per request");
        let taken = updates.next().await;
        assert_eq!(taken.len(), 2);
        assert!(taken.contains(&("phone".to_string(), 1, RelayRequestState::Accepted)));
    }

    #[test]
    fn result_texts_are_cut_to_the_bound_with_the_mark() {
        let text = bounded_text("é".repeat(RESULT_TEXT));
        assert!(text.len() <= RESULT_TEXT);
        assert!(text.ends_with('…'));
    }

    #[test]
    fn payloads_held_by_the_host_count_against_the_budget_until_let_go() {
        let mut f = fixture(2);
        let mut admission = SourceAdmission::new(Some(f.host.clone()));
        let big = |n: u64| ClientCommand::Prompt {
            text: format!("{n}{}", "p".repeat(60 * 1024)),
            disposition: Default::default(),
        };
        // The host keeps every request it receives (a queue of follow-ups).
        let mut held = Vec::new();
        let mut n = 0;
        loop {
            n += 1;
            match admission.decide("phone", n, big(n)) {
                None => {
                    held.push(f.incoming.try_recv().unwrap());
                    if held.len() == 8 {
                        // Drain the host channel so it is never what refuses.
                        continue;
                    }
                }
                Some(RelayRequestState::Rejected { .. }) => break,
                other => panic!("{other:?}"),
            }
            assert!(n < 100, "a refusal before the budget is crossed");
        }
        assert!(held.len() >= 10, "near-limit payloads up to the budget");
        assert!(admission.reserved() + admission.window.bytes() <= TABLE_BYTES);
        // The host lets requests go: their bytes come back and new ones fit.
        held.truncate(held.len() / 2);
        n += 1;
        assert_eq!(admission.decide("phone", n, big(n)), None);
    }
}
