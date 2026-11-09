//! The server's side of a relayed session: a full-screen session that another
//! local process runs and shares with paired devices.
//!
//! The server never opens, resumes or reads a relayed session. It registers
//! the relay after consistency checks against the session's owner record,
//! keeps a mirror of the source's feed (installed only from a complete,
//! verified snapshot, then advanced by the source's records in order), and
//! serves that mirror to devices. Devices reach only relayed sessions; every
//! other session of the workspace stays local.

use super::*;

use localpilot_rpc::{
    relayable, FeedCursor, RelayRecord, RelayRequestState, SnapshotAssembly, RELAY_RECORD_MAX,
};
use tokio::sync::oneshot;

/// Commands waiting to be written to a relay's source.
const FORWARD_QUEUE: usize = 64;
/// How long a device waits for the source to decide a command (accepted,
/// queued, rejected or done) before it is told the outcome is unknown. Work
/// the source accepted has no deadline.
const DECIDE_DEADLINE: Duration = Duration::from_secs(30);

/// Distinct devices a relayed session may have attached at once.
const ATTENDANCE_DEVICES: usize = 64;
/// Connections one device may have open to one relayed session.
const ATTENDANCE_SOCKETS: usize = 8;
/// How often an attended relay re-reads its devices' standing.
const ATTENDANCE_POLL: Duration = Duration::from_secs(1);
/// The latest attendance is written at least once per this many queued
/// command records.
const TELEMETRY_EVERY: usize = 8;
/// Bounds on what a source is told about a device.
const DEVICE_NAME_CHARS: usize = 64;
const DEVICE_NAME_BYTES: usize = 256;
const DEVICE_ID_BYTES: usize = 64;

/// Which devices are attached to one relayed session: open connections per
/// device, and how many devices arrived and left. Membership only; whether a
/// device is still allowed is read from the registry when it is reported.
pub(super) struct Attendance {
    state: StdMutex<AttendanceState>,
    changes: tokio::sync::watch::Sender<u64>,
}

#[derive(Default)]
struct AttendanceState {
    sockets: std::collections::BTreeMap<String, usize>,
    attaches: u64,
    detaches: u64,
}

impl Attendance {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: StdMutex::new(AttendanceState::default()),
            changes: tokio::sync::watch::channel(0).0,
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, AttendanceState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Admit one connection of `device`, or say why not. The guard releases it.
    pub(super) fn admit(self: &Arc<Self>, device: &str) -> Result<AttendanceGuard, &'static str> {
        {
            let mut state = self.state();
            let distinct = state.sockets.len();
            match state.sockets.get_mut(device) {
                Some(open) if *open >= ATTENDANCE_SOCKETS => {
                    return Err("this device has too many connections to this shared session");
                }
                Some(open) => *open += 1,
                None if distinct >= ATTENDANCE_DEVICES => {
                    return Err("this shared session has too many devices attached");
                }
                None => {
                    state.sockets.insert(device.to_string(), 1);
                    state.attaches += 1;
                }
            }
        }
        self.changes.send_modify(|n| *n += 1);
        Ok(AttendanceGuard {
            attendance: self.clone(),
            device: device.to_string(),
        })
    }

    /// The attached device ids and the arrival and departure counts.
    fn snapshot(&self) -> (Vec<String>, u64, u64) {
        let state = self.state();
        (
            state.sockets.keys().cloned().collect(),
            state.attaches,
            state.detaches,
        )
    }
}

/// One admitted device connection; dropping it ends that membership.
pub(super) struct AttendanceGuard {
    attendance: Arc<Attendance>,
    device: String,
}

impl Drop for AttendanceGuard {
    fn drop(&mut self) {
        {
            let mut state = self.attendance.state();
            if let Some(open) = state.sockets.get_mut(&self.device) {
                *open -= 1;
                if *open == 0 {
                    state.sockets.remove(&self.device);
                    state.detaches += 1;
                }
            }
        }
        self.attendance.changes.send_modify(|n| *n += 1);
    }
}

/// Text cut to at most `chars` characters and `bytes` bytes, with control
/// characters dropped.
fn bounded_label(text: &str, chars: usize, bytes: usize) -> String {
    let mut out = String::new();
    for c in text.chars().filter(|c| !c.is_control()).take(chars) {
        if out.len() + c.len_utf8() > bytes {
            break;
        }
        out.push(c);
    }
    out
}

/// What the source is told about the attached devices `ids`, from one read
/// of the registry: each one's standing and, while it is known, its name and
/// scope. When the registry cannot be read, every device is still listed
/// (membership is known) with an unknown standing and no metadata.
fn attended(registry: &DeviceRegistry, ids: &[String]) -> Vec<localpilot_rpc::AttendedDevice> {
    use localpilot_rpc::DeviceStanding;
    let id_of = |id: &String| bounded_label(id, DEVICE_ID_BYTES, DEVICE_ID_BYTES);
    let Some(standings) = registry.standings(ids) else {
        return ids
            .iter()
            .map(|id| localpilot_rpc::AttendedDevice {
                id: id_of(id),
                name: None,
                scope: None,
                standing: DeviceStanding::Unknown,
            })
            .collect();
    };
    ids.iter()
        .zip(standings)
        .map(|(id, (record, standing))| localpilot_rpc::AttendedDevice {
            id: id_of(id),
            name: record
                .as_ref()
                .map(|record| bounded_label(&record.name, DEVICE_NAME_CHARS, DEVICE_NAME_BYTES)),
            scope: record.as_ref().map(|record| record.scope),
            standing: match standing {
                Ok(_) => DeviceStanding::Active,
                Err(Refusal::NotGranted) => DeviceStanding::NotGranted,
                Err(Refusal::Revoked) => DeviceStanding::Revoked,
                Err(Refusal::BadCredential) => DeviceStanding::Removed,
                Err(Refusal::Unavailable) => DeviceStanding::Unknown,
            },
        })
        .collect()
}

/// One `relay_devices` record as bytes on a line. A record over the relay
/// cap is replaced, before anything is written, by one that says the list is
/// unknown (same binding, version and counts).
fn telemetry_line(
    relayed: &RelayedSession,
    version: u64,
    baseline: bool,
    (devices, attaches, detaches): &(Option<Vec<localpilot_rpc::AttendedDevice>>, u64, u64),
) -> Vec<u8> {
    let line = |devices: Option<Vec<localpilot_rpc::AttendedDevice>>| {
        let record = ServerRecord {
            v: RPC_PROTOCOL_VERSION,
            id: None,
            seq: None,
            event: ServerEvent::RelayDevices {
                relay: relayed.relay.clone(),
                share: relayed.share.clone(),
                version,
                baseline,
                attaches: *attaches,
                detaches: *detaches,
                devices,
            },
        };
        serde_json::to_vec(&record).ok().map(|mut bytes| {
            bytes.push(b'\n');
            bytes
        })
    };
    match line(devices.clone()) {
        Some(bytes) if bytes.len() < RELAY_RECORD_MAX => bytes,
        _ => line(None).unwrap_or_default(),
    }
}

/// A device command on its way to the source.
pub(super) struct Forward {
    device: String,
    command: ClientCommand,
    decided: oneshot::Sender<RelayRequestState>,
    /// The device's retry key, when it sent one: a decision that arrives after
    /// the device stopped waiting is recorded under it, so a retry gets the
    /// real outcome instead of `outcome_unknown`.
    late: Option<LateReply>,
}

/// Commands sent to the source and not yet decided, by device and number.
type Waiting = HashMap<(String, u64), (oneshot::Sender<RelayRequestState>, Option<LateReply>)>;

/// Record a decided command: first in the request store under its retry key
/// (so the outcome is known whatever happens to the waiting device), then
/// tell the device.
fn settle(
    requests: &RequestStore,
    session: SessionId,
    decided: oneshot::Sender<RelayRequestState>,
    late: Option<LateReply>,
    state: RelayRequestState,
) {
    if let Some(late) = late {
        let reply = reply_for(state.clone(), late.name, late.disposition, true);
        requests.complete(session, &late.stream, &late.key, reply);
    }
    let _ = decided.send(state);
}

/// Where a late decision is recorded.
struct LateReply {
    stream: String,
    key: localpilot_rpc::RequestKey,
    name: Option<String>,
    /// A prompt's requested disposition, for its `queued` acknowledgement.
    disposition: InputDisposition,
}

/// One relayed session, while its source's connection lives.
pub(super) struct RelayedSession {
    pub(super) session: SessionId,
    /// This registration's id, minted by the server.
    pub(super) relay: String,
    /// The mirror: its projection, history and admission are one state.
    pub(super) feed: Arc<SessionFeed>,
    /// The source's sharing generation this relay belongs to.
    share: String,
    /// Device commands for the source.
    forward: tokio::sync::mpsc::Sender<Forward>,
    /// How long a device waits for the source's first decision.
    deadline: Duration,
    /// The devices attached to this relay (never another relay's).
    pub(super) attendance: Arc<Attendance>,
    /// Phone notifications for what this mirror commits, when on.
    pub(super) push: Option<Arc<crate::remote_push_send::PushSender>>,
    /// Tests: the writer waits for a permit before it starts.
    #[cfg(test)]
    hold: Option<Arc<tokio::sync::Semaphore>>,
}

impl HostMap {
    /// The relayed session a device's attach target names, if it is shared.
    pub(super) fn relayed(&self, target: &AttachTarget) -> Option<Arc<RelayedSession>> {
        let relays = self.relays.lock().unwrap_or_else(PoisonError::into_inner);
        match target {
            AttachTarget::ResumeId { session_id } => relays.get(session_id).cloned(),
            // A device never opens a session, and names are not shared.
            _ => None,
        }
    }

    /// Every relayed session, as a device listing shows it.
    pub(super) fn relayed_listing(&self) -> Vec<localpilot_rpc::SessionInfo> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let mut items: Vec<_> = self
            .relays
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .map(|relayed| localpilot_rpc::SessionInfo {
                id: relayed.session,
                name: None,
                live: true,
                message_count: 0,
                updated_unix: now,
            })
            .collect();
        items.sort_by_key(|item| item.id.to_string());
        items
    }
}

/// Why a relay registration is refused, if it is.
fn registration_refusal(
    hosts: &HostMap,
    store: &Store,
    session: SessionId,
    owner_generation: u64,
    holder_instance: &str,
) -> Option<String> {
    if let Some(reason) = &hosts.remote.refused {
        return Some(reason.clone());
    }
    // A consistency check under same-user trust, not proof of the sender: the
    // process sharing a session must be the one its owner record names, and it
    // must still hold the session.
    let owned = matches!(
        store.owner_record(session),
        Ok(Some(record))
            if record.generation == owner_generation
                && record.holder.instance == holder_instance
                && record.transfer.is_none()
    );
    if !owned || !matches!(store.session_is_held(session), Ok(true)) {
        return Some("this session is not owned by the process sharing it".to_string());
    }
    None
}

/// Serve a relay connection: register it, keep the mirror, and end it when
/// the source stops sharing or goes away.
#[allow(clippy::too_many_arguments)] // one registration's whole context
pub(super) async fn serve_relay<R, W>(
    reader: &mut JsonRecordReader<R>,
    write: &mut W,
    hosts: &HostMap,
    store: &Store,
    reply_id: Option<String>,
    session: SessionId,
    owner_generation: u64,
    holder_instance: &str,
    share: &str,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (forward, forwarded) = tokio::sync::mpsc::channel(FORWARD_QUEUE);
    if let Some(message) =
        registration_refusal(hosts, store, session, owner_generation, holder_instance)
    {
        emit(write, reply_id, ServerEvent::Error { message }).await?;
        return Ok(());
    }
    let registered = {
        let mut relays = hosts.relays.lock().unwrap_or_else(PoisonError::into_inner);
        match relays.entry(session) {
            std::collections::hash_map::Entry::Occupied(_) => None,
            std::collections::hash_map::Entry::Vacant(slot) => {
                let relayed = Arc::new(RelayedSession {
                    session,
                    relay: localpilot_core::EventId::new().to_string(),
                    feed: SessionFeed::mirror(),
                    share: share.to_string(),
                    forward,
                    deadline: hosts.relay_deadline.unwrap_or(DECIDE_DEADLINE),
                    attendance: Attendance::new(),
                    push: hosts.push_sender.clone(),
                    #[cfg(test)]
                    hold: hosts.telemetry_hold.clone(),
                });
                slot.insert(relayed.clone());
                Some(relayed)
            }
        }
    };
    let Some(relayed) = registered else {
        emit(
            write,
            reply_id,
            ServerEvent::Error {
                message: "this session is already shared".to_string(),
            },
        )
        .await?;
        return Ok(());
    };
    let outcome = mirror(
        reader,
        write,
        &relayed,
        reply_id,
        forwarded,
        &hosts.requests,
        &hosts.devices,
    )
    .await;
    {
        let mut relays = hosts.relays.lock().unwrap_or_else(PoisonError::into_inner);
        if relays
            .get(&session)
            .is_some_and(|current| current.relay == relayed.relay)
        {
            relays.remove(&session);
        }
    }
    if let Some(push) = &relayed.push {
        push.observe(session, crate::remote_push_send::Mirror::Gone);
    }
    let reason = match &outcome {
        Ok(Some(reason)) => reason.clone(),
        Ok(None) | Err(_) => "the session on the PC was closed".to_string(),
    };
    relayed.feed.close(&reason);
    outcome.map(|_| ())
}

/// Read the source's records into the mirror. Returns the closure reason when
/// the source ended sharing itself, `None` when its connection ended.
#[allow(clippy::too_many_arguments)] // one relay's whole context
async fn mirror<R, W>(
    reader: &mut JsonRecordReader<R>,
    write: &mut W,
    relayed: &RelayedSession,
    reply_id: Option<String>,
    mut forwarded: tokio::sync::mpsc::Receiver<Forward>,
    requests: &RequestStore,
    registry: &DeviceRegistry,
) -> anyhow::Result<Option<String>>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // Devices may already queue commands while this is written: whatever
    // happens from here on, the epilogue below settles them.
    let ready = emit(
        write,
        reply_id,
        ServerEvent::RelayReady {
            relay: relayed.relay.clone(),
        },
    )
    .await;
    let mut waiting: Waiting = HashMap::new();
    let outcome = match ready {
        Ok(()) => {
            serve_mirror(
                reader,
                write,
                relayed,
                &mut forwarded,
                requests,
                &mut waiting,
                registry,
            )
            .await
        }
        Err(error) => Err(error.into()),
    };
    settle_all(&mut forwarded, &mut waiting, requests, relayed.session);
    outcome
}

/// The mirror's life after `relay_ready`: the source's records in, the
/// devices' commands out.
#[allow(clippy::too_many_arguments)] // one relay's whole context
async fn serve_mirror<R, W>(
    reader: &mut JsonRecordReader<R>,
    write: &mut W,
    relayed: &RelayedSession,
    forwarded: &mut tokio::sync::mpsc::Receiver<Forward>,
    requests: &RequestStore,
    waiting: &mut Waiting,
    registry: &DeviceRegistry,
) -> anyhow::Result<Option<String>>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    // Every relay record is held to the relay cap on its raw bytes, before
    // parsing.
    reader.set_max_record_len(RELAY_RECORD_MAX);
    // Commands for the source go through their own bounded, ordered writer,
    // so a source that stops reading never stops this side from reading its
    // records (its stop, its end, its snapshots and results).
    let (outgoing, mut to_write) = tokio::sync::mpsc::channel::<Vec<u8>>(FORWARD_QUEUE);
    // The writer also tells the source which devices are attached: first an
    // immutable baseline (version 1, nobody attached yet: the relay was just
    // created), then the latest state in one coalescing slot, never holding
    // back command records and written at least once per few of them. Every
    // record is written whole; a failed write ends the relay.
    let attendance = relayed.attendance.clone();
    let writer = async {
        #[cfg(test)]
        if let Some(hold) = &relayed.hold {
            let _ = hold.acquire().await;
        }
        let mut changes = attendance.changes.subscribe();
        let mut published: (Option<Vec<localpilot_rpc::AttendedDevice>>, u64, u64) =
            (Some(Vec::new()), 0, 0);
        let mut version = 1;
        let baseline = telemetry_line(relayed, version, true, &published);
        write.write_all(&baseline).await?;
        write.flush().await?;
        let mut pending: Option<Vec<u8>> = None;
        let mut since = 0usize;
        let mut next_poll = tokio::time::Instant::now() + ATTENDANCE_POLL;
        let mut refresh = |pending: &mut Option<Vec<u8>>| {
            let (ids, attaches, detaches) = attendance.snapshot();
            let current = (Some(attended(registry, &ids)), attaches, detaches);
            if current != published {
                version += 1;
                *pending = Some(telemetry_line(relayed, version, false, &current));
                published = current;
            }
        };
        // Whatever happened before this writer started (a device may have
        // attached, read and left already) is sampled once after the
        // baseline, so it is reported as the first update.
        refresh(&mut pending);
        // Each round does, in this order: at most one coalesced attendance
        // refresh (a change seen, or the standing poll due while anyone is
        // attached), the pending update when due, then one command record or
        // a wait. Neither attendance churn nor command traffic can starve the
        // other, and the poll runs at its time under sustained traffic.
        loop {
            let now = tokio::time::Instant::now();
            let poll_due = now >= next_poll;
            if poll_due {
                next_poll = now + ATTENDANCE_POLL;
            }
            if changes.has_changed().unwrap_or(false) {
                changes.mark_unchanged();
                refresh(&mut pending);
            } else if poll_due && !attendance.snapshot().0.is_empty() {
                refresh(&mut pending);
            }
            if pending.is_some() && (since >= TELEMETRY_EVERY || to_write.is_empty()) {
                if let Some(bytes) = pending.take() {
                    write.write_all(&bytes).await?;
                    write.flush().await?;
                }
                since = 0;
            }
            match to_write.try_recv() {
                Ok(bytes) => {
                    write.write_all(&bytes).await?;
                    write.flush().await?;
                    since += 1;
                    continue;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
            }
            tokio::select! {
                changed = changes.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    refresh(&mut pending);
                }
                bytes = to_write.recv() => {
                    let Some(bytes) = bytes else { break };
                    write.write_all(&bytes).await?;
                    write.flush().await?;
                    since += 1;
                }
                () = tokio::time::sleep_until(next_poll) => {}
            }
        }
        Ok::<(), std::io::Error>(())
    };
    let reading = read_source(reader, relayed, forwarded, outgoing, requests, waiting);
    let outcome = tokio::select! {
        outcome = reading => outcome,
        written = writer => match written {
            // The source's connection failed while being written to.
            Err(_) => Ok(None),
            Ok(()) => Ok(None),
        },
    };
    outcome
}

/// The relay ended: nothing more may be queued, and every command still
/// queued for the source or waiting for its decision is settled as unknown,
/// so none is left "still being handled" by a relay that is gone. Nothing is
/// sent to the source again.
fn settle_all(
    forwarded: &mut tokio::sync::mpsc::Receiver<Forward>,
    waiting: &mut Waiting,
    requests: &RequestStore,
    session: SessionId,
) {
    forwarded.close();
    while let Ok(Forward { decided, late, .. }) = forwarded.try_recv() {
        settle(requests, session, decided, late, RelayRequestState::Unknown);
    }
    settle_unknown(waiting, requests, session);
}

/// The relay ended: it can never decide what is still waiting, so each is
/// settled as unknown (a retry is told so rather than "still being handled").
fn settle_unknown(waiting: &mut Waiting, requests: &RequestStore, session: SessionId) {
    for (_, (decided, late)) in waiting.drain() {
        settle(requests, session, decided, late, RelayRequestState::Unknown);
    }
}

/// Read the source's records into the mirror and hand device commands to the
/// writer.
async fn read_source<R>(
    reader: &mut JsonRecordReader<R>,
    relayed: &RelayedSession,
    forwarded: &mut tokio::sync::mpsc::Receiver<Forward>,
    outgoing: tokio::sync::mpsc::Sender<Vec<u8>>,
    requests: &RequestStore,
    waiting: &mut Waiting,
) -> anyhow::Result<Option<String>>
where
    R: AsyncRead + Unpin,
{
    let mut assembly: Option<SnapshotAssembly> = None;
    // The source position the mirror follows: its epoch and last record.
    let mut position: Option<(String, u64)> = None;
    let mut next_n: u64 = 0;
    loop {
        let read = tokio::select! {
            read = reader.next() => read,
            forward = forwarded.recv() => {
                let Some(Forward { device, command, decided, late }) = forward else {
                    continue;
                };
                let refuse = |reason: &str| RelayRequestState::Rejected {
                    reason: reason.to_string(),
                };
                if waiting.len() >= FORWARD_QUEUE {
                    settle(requests, relayed.session, decided, late, refuse("too many requests are waiting"));
                    continue;
                }
                next_n += 1;
                let record = ServerRecord {
                    v: RPC_PROTOCOL_VERSION,
                    id: None,
                    seq: None,
                    event: ServerEvent::RelayCommand {
                        relay: relayed.relay.clone(),
                        share: relayed.share.clone(),
                        device: device.clone(),
                        n: next_n,
                        command: Box::new(command),
                    },
                };
                let Ok(mut bytes) = serde_json::to_vec(&record) else {
                    settle(requests, relayed.session, decided, late, refuse("the request could not be encoded"));
                    continue;
                };
                if bytes.len() >= RELAY_RECORD_MAX {
                    settle(requests, relayed.session, decided, late, refuse("the request is too large"));
                    continue;
                }
                bytes.push(b'\n');
                if outgoing.try_send(bytes).is_err() {
                    settle(requests, relayed.session, decided, late, refuse("the session is busy"));
                    continue;
                }
                waiting.insert((device, next_n), (decided, late));
                continue;
            }
        };
        let value = match read {
            Ok(Some(value)) => value,
            Ok(None) => break,
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                return Ok(Some("a relay record was too large".to_string()));
            }
            Err(error) => return Err(error.into()),
        };
        let Ok(record) = serde_json::from_value::<RelayRecord>(value) else {
            return Ok(Some("the relay sent an unreadable record".to_string()));
        };
        match record {
            RelayRecord::RelaySnapshotBegin {
                epoch,
                seq,
                parts,
                bytes,
            } => {
                // Whatever the mirror knew is replaced only once this snapshot
                // is complete; until then it is unknown.
                relayed.feed.forget();
                position = None;
                if let Some(push) = &relayed.push {
                    push.observe(relayed.session, crate::remote_push_send::Mirror::Gap);
                }
                assembly = match SnapshotAssembly::begin(epoch, seq, parts, bytes) {
                    Ok(assembly) => Some(assembly),
                    Err(error) => return Ok(Some(error.to_string())),
                };
            }
            RelayRecord::RelaySnapshotPart { index, data } => {
                let failed = match assembly.as_mut() {
                    Some(assembly) => assembly.part(index, &data).is_err(),
                    None => true,
                };
                if failed {
                    assembly = None;
                }
            }
            RelayRecord::RelaySnapshotCommit { digest } => {
                let Some(complete) = assembly.take() else {
                    continue;
                };
                let Ok(received) = complete.commit(&digest) else {
                    continue; // stays unknown until a snapshot verifies
                };
                if relayed.feed.install(&received.snapshot) {
                    position = Some((received.epoch, received.seq));
                    if let (Some(push), ServerEvent::Snapshot { asks, .. }) =
                        (&relayed.push, &received.snapshot)
                    {
                        let open = asks
                            .iter()
                            .map(|ask| (ask.ask_id.clone(), ask.expires_at_ms))
                            .collect();
                        push.observe(
                            relayed.session,
                            crate::remote_push_send::Mirror::Baseline(open),
                        );
                    }
                }
            }
            RelayRecord::RelayEvent { epoch, seq, event } => {
                let in_order = position
                    .as_ref()
                    .is_some_and(|(known, last)| *known == epoch && seq == last + 1);
                if !in_order {
                    // A gap or a record from another epoch: never guess.
                    if position.is_some() {
                        relayed.feed.forget();
                        position = None;
                        if let Some(push) = &relayed.push {
                            push.observe(relayed.session, crate::remote_push_send::Mirror::Gap);
                        }
                    }
                    continue;
                }
                position = Some((epoch.clone(), seq));
                if relayable(&event) {
                    if let Some(push) = &relayed.push {
                        use crate::remote_push_send::Mirror;
                        let fact = match &event {
                            ServerEvent::PermissionAsk {
                                ask_id,
                                expires_at_ms,
                                ..
                            } => Some(Mirror::AskOpened(ask_id.clone(), *expires_at_ms)),
                            ServerEvent::PermissionResolved { ask_id, .. } => {
                                Some(Mirror::AskEnded(ask_id.clone()))
                            }
                            ServerEvent::Stopped { .. } => Some(Mirror::Stopped(epoch, seq)),
                            _ => None,
                        };
                        if let Some(fact) = fact {
                            push.observe(relayed.session, fact);
                        }
                    }
                    relayed.feed.relay(event);
                }
            }
            RelayRecord::RelayStop => return Ok(Some("sharing stopped".to_string())),
            RelayRecord::RelayResult { device, n, state } => {
                // The first decision answers the device; later transitions of
                // the same request (a queued prompt starting) are the source's.
                if let Some((decided, late)) = waiting.remove(&(device, n)) {
                    settle(requests, relayed.session, decided, late, state);
                }
            }
            _ => return Ok(Some("the relay sent an unknown record".to_string())),
        }
    }
    Ok(None)
}

/// Serve a device attached to a relayed session: the mirror's records, under
/// the same access guards as any device connection. Commands are not carried
/// to the source yet; they are refused and nothing runs.
#[allow(clippy::too_many_arguments)] // one device connection's whole context
pub(super) async fn serve_relayed_device<R, W>(
    reader: &mut JsonRecordReader<R>,
    write: &mut W,
    hosts: &HostMap,
    reply_id: Option<String>,
    principal: Principal,
    lease: CancellationToken,
    relayed: Arc<RelayedSession>,
    capabilities: &[String],
    since: Option<FeedCursor>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let Principal::Device { id: device } = &principal else {
        return Ok(());
    };
    let acks = capabilities.iter().any(|name| name == CAPABILITY_ACKS);
    let resume = capabilities.iter().any(|name| name == CAPABILITY_RESUME);
    let authorized = || -> Result<(), String> {
        if lease.is_cancelled() {
            return Err("the gateway disconnected".to_string());
        }
        hosts
            .devices
            .standing(device)
            .map(|_| ())
            .map_err(|_| "this device is no longer allowed".to_string())
    };
    macro_rules! guarded {
        ($work:expr) => {
            match while_authorized(&lease, &authorized, $work).await {
                Some(result) => result?,
                None => return Ok(()),
            }
        };
    }
    // Attendance is bounded: a device over the bounds gets nothing of the
    // session. The guard ends this connection's membership however it ends.
    let _attending = match relayed.attendance.admit(device) {
        Ok(guard) => guard,
        Err(message) => {
            emit(
                write,
                reply_id,
                ServerEvent::Error {
                    message: message.to_string(),
                },
            )
            .await?;
            return Ok(());
        }
    };
    // A mirror's snapshots carry its source's history and admission; these
    // are never consulted for it.
    let read_history = || (Vec::new(), true);
    let read_admission = Admission::default;
    let attachment = relayed
        .feed
        .attach(since.as_ref(), resume, &read_history(), &read_admission);
    let mut live = attachment.live;
    let _guard = attachment.guard;
    if let Err(message) = authorized() {
        emit(write, reply_id, ServerEvent::Error { message }).await?;
        return Ok(());
    }
    let mut view = ConnectionView {
        acks,
        resume,
        floor: attachment.floor,
        shown: HashSet::new(),
    };
    guarded!(emit(
        write,
        reply_id,
        ServerEvent::Attached {
            capabilities: server_capabilities(),
            instance: hosts.requests.instance().to_string(),
            session_id: relayed.session,
            server_version: SERVER_VERSION.to_string(),
            epoch: if resume {
                attachment.epoch
            } else {
                String::new()
            },
        },
    ));
    let feed_ctx = FeedContext {
        feed: &relayed.feed,
        history: &read_history,
        admission: &read_admission,
    };
    for (seq, event) in attachment.initial {
        if guarded!(deliver(write, &mut view, &feed_ctx, seq, event)) == Flow::Stop {
            return Ok(());
        }
    }
    let mut standing_tick = tokio::time::interval(Duration::from_secs(1));
    standing_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = standing_tick.tick() => {
                if let Err(refusal) = hosts.devices.standing(device) {
                    let reason = match refusal {
                        Refusal::Revoked => "this device was revoked",
                        Refusal::NotGranted => "this device is no longer allowed in this workspace",
                        Refusal::Unavailable => "paired devices are disabled on this server",
                        Refusal::BadCredential => "this device is no longer paired",
                    };
                    farewell(write, &view, reason.to_string()).await;
                    break;
                }
            }
            () = lease.cancelled() => {
                farewell(write, &view, "the gateway disconnected".to_string()).await;
                break;
            }
            record = live.recv() => match record {
                Ok((seq, event)) => {
                    if seq <= view.floor {
                        continue;
                    }
                    if guarded!(deliver(write, &mut view, &feed_ctx, Some(seq), event))
                        == Flow::Stop
                    {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if guarded!(resync(write, &mut view, &feed_ctx)) == Flow::Stop {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            record = reader.next() => {
                let Some(value) = record? else { break };
                match authorize(hosts, &principal, &value) {
                    Authorized::Gone(reason) => {
                        farewell(write, &view, reason).await;
                        break;
                    }
                    Authorized::Refused(id, message) => {
                        guarded!(reject(write, id, acks, RejectCode::Forbidden, message));
                    }
                    Authorized::Yes => {
                        let replies = device_command(hosts, &relayed, device, acks, &value).await;
                        for (id, event) in replies {
                            guarded!(emit(write, id, event));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Register a relayed session directly, with a mirror installed from a
/// running-turn snapshot, for tests that serve one connection by hand.
#[cfg(test)]
pub(super) fn relayed_for_test(hosts: &HostMap) -> SessionId {
    let session = SessionId::new();
    let (forward, _forwarded) = tokio::sync::mpsc::channel(FORWARD_QUEUE);
    let relayed = Arc::new(RelayedSession {
        session,
        relay: localpilot_core::EventId::new().to_string(),
        feed: SessionFeed::mirror(),
        share: "test".to_string(),
        forward,
        deadline: DECIDE_DEADLINE,
        attendance: Attendance::new(),
        push: None,
        hold: None,
    });
    relayed.feed.install(&ServerEvent::Snapshot {
        epoch: "source".to_string(),
        seq: 1,
        turn: localpilot_rpc::TurnState::Running,
        busy: true,
        queued: 0,
        plan: None,
        asks: Vec::new(),
        turn_text: Some("a shared turn".to_string()),
        turn_text_truncated: false,
        history: Vec::new(),
        history_truncated: false,
    });
    hosts
        .relays
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(session, relayed);
    session
}

/// Answer one admitted device command on a relayed session: the listing of
/// remote commands here, the session's commands by forwarding them to the
/// source, everything else refused. A command with a retry key runs at most
/// once (a retry gets the recorded reply). Returns the records to send.
async fn device_command(
    hosts: &HostMap,
    relayed: &RelayedSession,
    device: &str,
    acks: bool,
    value: &serde_json::Value,
) -> Vec<(Option<String>, ServerEvent)> {
    let Ok(record) = serde_json::from_value::<ClientRecord>(value.clone()) else {
        return vec![(
            None,
            refusal(acks, RejectCode::Malformed, "the record could not be read"),
        )];
    };
    let id = record.id.clone();
    match &record.command {
        ClientCommand::Commands => {
            return vec![(
                id,
                ServerEvent::Commands {
                    items: remote_commands(),
                },
            )]
        }
        ClientCommand::Prompt { .. }
        | ClientCommand::Cancel
        | ClientCommand::PermissionReply { .. }
        | ClientCommand::RunCommand { .. } => {}
        _ => {
            return vec![(
                id,
                refusal(
                    acks,
                    RejectCode::Forbidden,
                    "not available on a shared session; nothing ran",
                ),
            )]
        }
    }
    let stream = format!("device:{device}");
    let keyed = retry_key(value);
    if let Some((_, key, payload)) = &keyed {
        if !acks {
            return vec![(
                id,
                refusal(
                    acks,
                    RejectCode::Malformed,
                    "a request key needs the `acks` capability; nothing ran",
                ),
            )];
        }
        let Ok(payload) = payload else {
            return vec![(
                id,
                refusal(
                    acks,
                    RejectCode::Malformed,
                    "the command could not be encoded",
                ),
            )];
        };
        match hosts.requests.admit(relayed.session, &stream, key, payload) {
            RequestAdmission::Run => {}
            RequestAdmission::Replay(reply) => {
                return reply.into_iter().map(|event| (id.clone(), event)).collect();
            }
            RequestAdmission::Refuse(code, message) => {
                return vec![(id, refusal(acks, code, &message))];
            }
        }
    }
    let name = match &record.command {
        ClientCommand::RunCommand { name, .. } => Some(name.clone()),
        _ => None,
    };
    let disposition = match &record.command {
        ClientCommand::Prompt { disposition, .. } => *disposition,
        _ => InputDisposition::default(),
    };
    let late = keyed.as_ref().map(|(_, key, _)| LateReply {
        stream: stream.clone(),
        key: key.clone(),
        name: name.clone(),
        disposition,
    });
    let state = forward(relayed, device, record.command, late).await;
    if matches!(state, RelayRequestState::Unknown) {
        // Not recorded: a decision that arrives later is recorded under the
        // key instead (until then a retry is told it is still being handled).
        return vec![(
            id,
            refusal(
                acks,
                RejectCode::OutcomeUnknown,
                "the PC did not say what happened to this request yet",
            ),
        )];
    }
    let reply = reply_for(state, name, disposition, acks);
    if let Some((_, key, _)) = &keyed {
        hosts
            .requests
            .complete(relayed.session, &stream, key, reply.clone());
    }
    reply.into_iter().map(|event| (id.clone(), event)).collect()
}

/// The records answering a decided command.
fn reply_for(
    state: RelayRequestState,
    name: Option<String>,
    disposition: InputDisposition,
    acks: bool,
) -> Vec<ServerEvent> {
    let event = match state {
        RelayRequestState::Accepted => acks.then_some(ServerEvent::Accepted {
            dropped_follow_ups: None,
        }),
        RelayRequestState::Queued => acks.then_some(ServerEvent::Queued { disposition }),
        RelayRequestState::Done { output } => match name {
            Some(name) => Some(ServerEvent::CommandResult {
                name,
                message: output.unwrap_or_default(),
            }),
            None => acks.then_some(ServerEvent::Accepted {
                dropped_follow_ups: None,
            }),
        },
        RelayRequestState::Rejected { reason } => {
            let code = if reason == "already answered" {
                RejectCode::AlreadyResolved
            } else {
                RejectCode::Forbidden
            };
            Some(refusal(acks, code, &reason))
        }
        RelayRequestState::Unknown => Some(refusal(
            acks,
            RejectCode::OutcomeUnknown,
            "the PC did not say what happened to this request",
        )),
        _ => None,
    };
    event.into_iter().collect()
}

/// A refusal in the record type the connection reads.
fn refusal(acks: bool, code: RejectCode, message: &str) -> ServerEvent {
    if acks {
        ServerEvent::Rejected {
            code,
            message: message.to_string(),
        }
    } else {
        ServerEvent::Error {
            message: message.to_string(),
        }
    }
}

/// Send a command to the source and wait for its first decision, at most
/// [`DECIDE_DEADLINE`]. Never re-sent: an undecided command is unknown.
async fn forward(
    relayed: &RelayedSession,
    device: &str,
    command: ClientCommand,
    late: Option<LateReply>,
) -> RelayRequestState {
    let (decided, decision) = oneshot::channel();
    let sent = relayed.forward.try_send(Forward {
        device: device.to_string(),
        command,
        decided,
        late,
    });
    if sent.is_err() {
        return RelayRequestState::Rejected {
            reason: "the session is busy".to_string(),
        };
    }
    match tokio::time::timeout(relayed.deadline, decision).await {
        Ok(Ok(state)) => state,
        // The relay ended, or the source did not decide in time.
        Ok(Err(_)) | Err(_) => RelayRequestState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyed(store: &RequestStore, session: SessionId, n: u64) -> (LateReply, Vec<u8>) {
        let key = localpilot_rpc::RequestKey {
            instance: store.instance().to_string(),
            client: "phone".to_string(),
            n,
        };
        let payload = b"prompt".to_vec();
        assert!(matches!(
            store.admit(session, "device:d1", &key, &payload),
            RequestAdmission::Run
        ));
        (
            LateReply {
                stream: "device:d1".to_string(),
                key,
                name: None,
                disposition: InputDisposition::default(),
            },
            payload,
        )
    }

    #[test]
    fn a_decision_is_recorded_whatever_happens_to_the_waiting_device() {
        let store = RequestStore::new();
        let session = SessionId::new();
        let (late, payload) = keyed(&store, session, 1);
        let key = late.key.clone();
        // The device's waiter exists when the decision arrives, but never
        // consumes it (its connection ends right after).
        let (decided, waiter) = oneshot::channel();
        settle(
            &store,
            session,
            decided,
            Some(late),
            RelayRequestState::Accepted,
        );
        drop(waiter);
        assert!(matches!(
            store.admit(session, "device:d1", &key, &payload),
            RequestAdmission::Replay(reply)
                if matches!(reply.as_slice(), [ServerEvent::Accepted { .. }])
        ));
    }

    #[test]
    fn requests_still_waiting_when_the_relay_ends_settle_as_unknown() {
        let store = RequestStore::new();
        let session = SessionId::new();
        let (late, payload) = keyed(&store, session, 1);
        let key = late.key.clone();
        let (decided, _waiter) = oneshot::channel();
        let mut waiting: Waiting = HashMap::new();
        waiting.insert(("d1".to_string(), 1), (decided, Some(late)));
        settle_unknown(&mut waiting, &store, session);
        assert!(waiting.is_empty());
        assert!(matches!(
            store.admit(session, "device:d1", &key, &payload),
            RequestAdmission::Replay(reply)
                if matches!(
                    reply.as_slice(),
                    [ServerEvent::Rejected { code: RejectCode::OutcomeUnknown, .. }]
                )
        ));
    }

    #[test]
    fn a_command_still_queued_when_the_relay_ends_settles_as_unknown() {
        let store = RequestStore::new();
        let session = SessionId::new();
        let (late, payload) = keyed(&store, session, 1);
        let key = late.key.clone();
        let (forward, mut forwarded) = tokio::sync::mpsc::channel(FORWARD_QUEUE);
        let (decided, mut waiter) = oneshot::channel();
        // Queued for the source; the relay ends before anything read it.
        forward
            .try_send(Forward {
                device: "d1".to_string(),
                command: ClientCommand::Cancel,
                decided,
                late: Some(late),
            })
            .unwrap();
        let mut waiting: Waiting = HashMap::new();
        settle_all(&mut forwarded, &mut waiting, &store, session);
        assert_eq!(waiter.try_recv(), Ok(RelayRequestState::Unknown));
        assert!(
            forward
                .try_send(Forward {
                    device: "d1".to_string(),
                    command: ClientCommand::Cancel,
                    decided: oneshot::channel().0,
                    late: None,
                })
                .is_err(),
            "nothing more is queued"
        );
        assert!(matches!(
            store.admit(session, "device:d1", &key, &payload),
            RequestAdmission::Replay(reply)
                if matches!(
                    reply.as_slice(),
                    [ServerEvent::Rejected { code: RejectCode::OutcomeUnknown, .. }]
                )
        ));
    }

    #[test]
    fn a_known_refusal_is_recorded_even_if_the_device_never_reads_it() {
        let store = RequestStore::new();
        let session = SessionId::new();
        let (late, payload) = keyed(&store, session, 1);
        let key = late.key.clone();
        let (decided, waiter) = oneshot::channel();
        settle(
            &store,
            session,
            decided,
            Some(late),
            RelayRequestState::Rejected {
                reason: "the session is busy".to_string(),
            },
        );
        drop(waiter);
        assert!(matches!(
            store.admit(session, "device:d1", &key, &payload),
            RequestAdmission::Replay(reply)
                if matches!(reply.as_slice(), [ServerEvent::Rejected { .. }])
        ));
    }

    /// A writer that always fails.
    struct Broken;

    impl tokio::io::AsyncWrite for Broken {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn a_command_queued_while_the_ready_write_fails_settles_as_unknown() {
        let store = RequestStore::new();
        let session = SessionId::new();
        let (late, payload) = keyed(&store, session, 1);
        let key = late.key.clone();
        let (forward, forwarded) = tokio::sync::mpsc::channel(FORWARD_QUEUE);
        let relayed = RelayedSession {
            session,
            relay: "r".to_string(),
            feed: SessionFeed::mirror(),
            share: "s".to_string(),
            forward: forward.clone(),
            deadline: DECIDE_DEADLINE,
            attendance: Attendance::new(),
            push: None,
            hold: None,
        };
        // A device queued its command during the handshake.
        let (decided, mut waiter) = oneshot::channel();
        forward
            .try_send(Forward {
                device: "d1".to_string(),
                command: ClientCommand::Cancel,
                decided,
                late: Some(late),
            })
            .unwrap();
        let mut reader = JsonRecordReader::new(tokio::io::empty());
        let outcome = mirror(
            &mut reader,
            &mut Broken,
            &relayed,
            None,
            forwarded,
            &store,
            &DeviceRegistry::default(),
        )
        .await;
        assert!(outcome.is_err(), "the ready write failed");
        assert_eq!(waiter.try_recv(), Ok(RelayRequestState::Unknown));
        assert!(matches!(
            store.admit(session, "device:d1", &key, &payload),
            RequestAdmission::Replay(reply)
                if matches!(
                    reply.as_slice(),
                    [ServerEvent::Rejected { code: RejectCode::OutcomeUnknown, .. }]
                )
        ));
    }

    #[test]
    fn a_queued_acknowledgement_carries_the_requested_disposition() {
        for disposition in [
            InputDisposition::Immediate,
            InputDisposition::Steer,
            InputDisposition::FollowUp,
        ] {
            assert_eq!(
                reply_for(RelayRequestState::Queued, None, disposition, true),
                vec![ServerEvent::Queued { disposition }]
            );
        }
    }

    /// A writer that takes `budget` bytes, then fails: a record can be cut
    /// off part-way.
    struct Cutoff {
        budget: usize,
    }

    impl tokio::io::AsyncWrite for Cutoff {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            if self.budget == 0 {
                return std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
            }
            let taken = buf.len().min(self.budget);
            self.budget -= taken;
            std::task::Poll::Ready(Ok(taken))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn a_telemetry_record_cut_off_part_way_ends_the_relay_with_its_cleanup() {
        let store = RequestStore::new();
        let session = SessionId::new();
        let (late, _payload) = keyed(&store, session, 1);
        let (forward, forwarded) = tokio::sync::mpsc::channel(FORWARD_QUEUE);
        let relayed = RelayedSession {
            session,
            relay: "r".to_string(),
            feed: SessionFeed::mirror(),
            share: "s".to_string(),
            forward: forward.clone(),
            deadline: DECIDE_DEADLINE,
            attendance: Attendance::new(),
            push: None,
            hold: None,
        };
        let (decided, mut waiter) = oneshot::channel();
        forward
            .try_send(Forward {
                device: "d1".to_string(),
                command: ClientCommand::Cancel,
                decided,
                late: Some(late),
            })
            .unwrap();
        // `relay_ready` goes through whole; the baseline after it is cut off.
        let ready = {
            let record = ServerRecord {
                v: RPC_PROTOCOL_VERSION,
                id: None,
                seq: None,
                event: ServerEvent::RelayReady {
                    relay: "r".to_string(),
                },
            };
            serde_json::to_vec(&record).unwrap().len() + 1
        };
        let (_peer, quiet) = tokio::io::duplex(64);
        let mut reader = JsonRecordReader::new(quiet);
        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            mirror(
                &mut reader,
                &mut Cutoff { budget: ready + 10 },
                &relayed,
                None,
                forwarded,
                &store,
                &DeviceRegistry::default(),
            ),
        )
        .await
        .expect("the relay ended");
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(
            waiter.try_recv(),
            Ok(RelayRequestState::Unknown),
            "cleaned up"
        );
    }

    #[test]
    fn an_attendance_record_over_the_cap_is_sent_as_unknown_with_its_binding() {
        let (forward, _forwarded) = tokio::sync::mpsc::channel(1);
        let relayed = RelayedSession {
            session: SessionId::new(),
            relay: "r".to_string(),
            feed: SessionFeed::mirror(),
            share: "s".to_string(),
            forward,
            deadline: DECIDE_DEADLINE,
            attendance: Attendance::new(),
            push: None,
            hold: None,
        };
        let many: Vec<_> = (0..5000)
            .map(|n| localpilot_rpc::AttendedDevice {
                id: format!("{n:064}"),
                name: Some("n".repeat(DEVICE_NAME_CHARS)),
                scope: Some(localpilot_rpc::Scope::Control),
                standing: localpilot_rpc::DeviceStanding::Active,
            })
            .collect();
        let line = telemetry_line(&relayed, 7, false, &(Some(many), 9, 4));
        assert!(line.len() < RELAY_RECORD_MAX);
        let record: ServerRecord = serde_json::from_slice(&line).unwrap();
        assert_eq!(
            record.event,
            ServerEvent::RelayDevices {
                relay: "r".to_string(),
                share: "s".to_string(),
                version: 7,
                baseline: false,
                attaches: 9,
                detaches: 4,
                devices: None,
            }
        );
    }

    #[test]
    fn attendance_is_bounded_per_device_and_per_relay_and_frees_with_the_last() {
        let attendance = Attendance::new();
        let one: Vec<_> = (0..ATTENDANCE_SOCKETS)
            .map(|_| attendance.admit("d").unwrap())
            .collect();
        assert!(attendance.admit("d").is_err(), "a ninth connection");
        let others: Vec<_> = (1..ATTENDANCE_DEVICES)
            .map(|n| attendance.admit(&format!("o{n}")).unwrap())
            .collect();
        assert!(attendance.admit("new").is_err(), "a 65th device");
        assert_eq!(attendance.snapshot().1, ATTENDANCE_DEVICES as u64);
        drop(one);
        assert_eq!(attendance.snapshot().2, 1, "left with its last connection");
        let admitted = attendance.admit("new");
        assert!(admitted.is_ok(), "room again");
        drop((others, admitted));
        assert!(attendance.snapshot().0.is_empty());
    }

    #[test]
    fn device_labels_are_bounded_and_carry_no_control_characters() {
        let name = format!("a\u{1b}[2Jb{}", "é".repeat(400));
        let label = bounded_label(&name, DEVICE_NAME_CHARS, DEVICE_NAME_BYTES);
        assert!(!label.chars().any(char::is_control));
        assert!(label.chars().count() <= DEVICE_NAME_CHARS && label.len() <= DEVICE_NAME_BYTES);
    }
}
