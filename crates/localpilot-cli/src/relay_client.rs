//! The source side of a relay: publish this process's session feed to the
//! workspace server so paired devices can follow the session.
//!
//! [`run_relay`] registers the session (`relay_host`), sends a snapshot of the
//! local feed at one position in verified parts, then every later feed record
//! in order. A local reader that falls behind (or a local feed that lost
//! records) sends a fresh snapshot; too many of those in a short time ends
//! sharing rather than flooding the server. Nothing here borrows the runtime:
//! the feed is the session's own, kept from its first event.
//!
//! The full-screen `/remote share` command runs it (see `remote_share`).
#![cfg_attr(not(any(test, feature = "tui")), allow(dead_code))]

use std::sync::Arc;
use std::time::{Duration, Instant};

use localpilot_core::SessionId;
use localpilot_rpc::{
    relayable, snapshot_records, ClientCommand, ClientRecord, JsonRecordReader, RelayError,
    RelayRecord, RelayRequestState, ServerEvent, ServerRecord, RELAY_RECORD_MAX,
    RPC_PROTOCOL_VERSION,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::relay_host::{SourceAdmission, SourceHost};
use crate::server_feed::SessionFeed;

/// More fresh snapshots than this within [`RESNAPSHOT_WINDOW`] ends sharing.
const RESNAPSHOT_LIMIT: usize = 3;
const RESNAPSHOT_WINDOW: Duration = Duration::from_secs(60);

/// Who is sharing which session.
#[derive(Debug, Clone)]
pub(crate) struct RelayIdentity {
    pub(crate) session: SessionId,
    pub(crate) owner_generation: u64,
    pub(crate) holder_instance: String,
    /// This share's generation, new on every share.
    pub(crate) share: String,
}

/// Why a relay ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RelayEnd {
    /// Sharing was stopped here.
    Stopped,
    /// The server refused the registration.
    Refused(String),
    /// The server's connection ended.
    ServerGone,
    /// The local session ended or changed.
    SessionEnded,
    /// The session's state does not fit the relay.
    TooLarge,
    /// The relay kept falling behind.
    Unstable,
}

async fn send<W: AsyncWrite + Unpin>(
    write: &mut W,
    value: &impl serde::Serialize,
) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    line.push(b'\n');
    write.write_all(&line).await?;
    write.flush().await
}

/// How long a closing `relay_stop` may take before the connection is simply
/// dropped (a server that stopped reading never holds sharing open).
const STOP_FLUSH: Duration = Duration::from_millis(200);

/// A snapshot of the local feed: its position, how many losses the feed had
/// when it was taken, and the records that carry it.
struct Taken {
    position: (String, u64),
    losses: u64,
    records: Vec<RelayRecord>,
}

/// A snapshot of the local feed as the records that carry it. History and
/// admission are the host's, read with the projection under the feed's lock.
fn snapshot_now(feed: &SessionFeed) -> Result<Taken, RelayEnd> {
    if !feed.shareable() {
        return Err(RelayEnd::TooLarge);
    }
    let (snapshot, losses) = feed.source_snapshot();
    let ServerEvent::Snapshot { epoch, seq, .. } = &snapshot else {
        return Err(RelayEnd::SessionEnded);
    };
    let position = (epoch.clone(), *seq);
    match snapshot_records(&snapshot) {
        Ok(records) => Ok(Taken {
            position,
            losses,
            records,
        }),
        Err(RelayError::SnapshotTooLarge) => Err(RelayEnd::TooLarge),
        Err(_) => Err(RelayEnd::SessionEnded),
    }
}

/// Publish the session behind `feed` over `conn` until `stop` fires, the server
/// goes away, or the session ends. `ready` fires once the server accepted the
/// registration and the first snapshot was sent: only then is the session
/// shared.
///
/// Every write races `stop` and the session ending, so a server that stops
/// reading never holds the source: stopping drops the connection after at
/// most a short best-effort `relay_stop`.
pub(crate) async fn run_relay<S>(
    conn: S,
    feed: Arc<SessionFeed>,
    identity: &RelayIdentity,
    stop: &CancellationToken,
    host: Option<SourceHost>,
    ready: Option<tokio::sync::oneshot::Sender<()>>,
) -> std::io::Result<RelayEnd>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Commands from devices are decided here, finally; every state the host
    // reports later arrives on `updates`. Dropping the admission (any return)
    // ends sharing for requests still queued.
    let attendance = host.as_ref().and_then(|host| host.attendance.clone());
    let mut admission = SourceAdmission::new(host);
    let updates = admission.updates();
    let (read, mut write) = tokio::io::split(conn);
    let mut reader = JsonRecordReader::new(read);
    let ended = feed.closed_signal();
    // One write, raced against stopping and the session ending.
    macro_rules! write_or_end {
        ($record:expr) => {
            tokio::select! {
                biased;
                () = stop.cancelled() => return Ok(RelayEnd::Stopped),
                () = ended.cancelled() => return Ok(RelayEnd::SessionEnded),
                written = send(&mut write, $record) => written?,
            }
        };
    }
    // End the relay, telling the server if it is still reading.
    macro_rules! finish {
        ($end:expr) => {{
            let _ =
                tokio::time::timeout(STOP_FLUSH, send(&mut write, &RelayRecord::RelayStop)).await;
            return Ok($end);
        }};
    }
    let registration = ClientRecord {
        v: RPC_PROTOCOL_VERSION,
        id: None,
        request: None,
        command: ClientCommand::RelayHost {
            session: identity.session,
            owner_generation: identity.owner_generation,
            holder_instance: identity.holder_instance.clone(),
            share: identity.share.clone(),
        },
    };
    write_or_end!(&registration);
    let reply = tokio::select! {
        biased;
        () = stop.cancelled() => return Ok(RelayEnd::Stopped),
        () = ended.cancelled() => return Ok(RelayEnd::SessionEnded),
        reply = reader.next() => reply?,
    };
    let Some(reply) = reply else {
        return Ok(RelayEnd::ServerGone);
    };
    let relay = match serde_json::from_value::<ServerRecord>(reply).map(|record| record.event) {
        Ok(ServerEvent::RelayReady { relay }) => relay,
        Ok(ServerEvent::Error { message }) => return Ok(RelayEnd::Refused(message)),
        _ => return Ok(RelayEnd::Refused("unexpected reply".to_string())),
    };
    // A registration that completes after sharing was stopped is withdrawn at
    // once.
    if stop.is_cancelled() {
        finish!(RelayEnd::Stopped);
    }

    // Subscribe before the snapshot so nothing between them is missed; live
    // records at or below the snapshot's position are already in it.
    let mut live = feed.live();
    let mut changes = feed.source_changes();
    changes.mark_unchanged();
    let Taken {
        position: (mut epoch, mut floor),
        losses: mut seen_losses,
        records,
    } = match snapshot_now(&feed) {
        Ok(taken) => taken,
        Err(end) => finish!(end),
    };
    for record in &records {
        write_or_end!(record);
    }
    if let Some(ready) = ready {
        let _ = ready.send(());
    }
    let mut resnapshots: Vec<Instant> = Vec::new();
    // The attendance version accepted last: none until the baseline.
    let mut attendance_version: Option<u64> = None;
    loop {
        // Why the next snapshot is sent: `Some(true)` after a loss seen here
        // (rate limited), `Some(false)` when the feed moved to a new epoch or
        // the host published new history or admission (a loss there is
        // recognised from the feed's own loss count).
        let mut resnapshot: Option<bool> = None;
        tokio::select! {
            biased;
            () = stop.cancelled() => finish!(RelayEnd::Stopped),
            () = ended.cancelled() => finish!(RelayEnd::SessionEnded),
            changed = changes.changed() => {
                if changed.is_err() {
                    finish!(RelayEnd::SessionEnded);
                }
                resnapshot = Some(false);
            },
            reply = reader.next() => match reply {
                Ok(Some(value)) => {
                    let event = serde_json::from_value::<ServerRecord>(value).map(|record| record.event);
                    // Who is attached: only for this relay and share, the
                    // baseline first, then strictly newer versions.
                    if let Ok(ServerEvent::RelayDevices {
                        relay: to,
                        share,
                        version,
                        baseline,
                        attaches,
                        detaches,
                        devices,
                    }) = &event
                    {
                        let current = *to == relay && *share == identity.share;
                        let next = match attendance_version {
                            None => *baseline && *version == 1,
                            Some(seen) => !*baseline && *version > seen,
                        };
                        if current && next {
                            attendance_version = Some(*version);
                            if let Some(report) = &attendance {
                                report(crate::relay_host::Telemetry {
                                    version: *version,
                                    baseline: *baseline,
                                    attaches: *attaches,
                                    detaches: *detaches,
                                    devices: devices.clone(),
                                });
                            }
                        }
                        continue;
                    }
                    if let Ok(ServerEvent::RelayCommand {
                        relay: to,
                        share,
                        device,
                        n,
                        command,
                    }) = event
                    {
                        // A command for another relay or an earlier share never
                        // runs here.
                        let state = if to != relay || share != identity.share {
                            Some(RelayRequestState::Unknown)
                        } else {
                            admission.decide(&device, n, *command)
                        };
                        if let Some(state) = state {
                            let result = RelayRecord::RelayResult { device, n, state };
                            write_or_end!(&result);
                        }
                    }
                    continue;
                }
                Ok(None) | Err(_) => return Ok(RelayEnd::ServerGone),
            },
            reported = updates.next() => {
                for (device, n, state) in reported {
                    admission.note(&device, n, state.clone());
                    let result = RelayRecord::RelayResult { device, n, state };
                    write_or_end!(&result);
                }
                continue;
            },
            record = live.recv() => match record {
                Ok((seq, event)) => {
                    if seq <= floor {
                        continue;
                    }
                    match event {
                        ServerEvent::SessionClosed { .. } => finish!(RelayEnd::SessionEnded),
                        // The local feed moved to a new epoch: rebuilt after
                        // a loss, or refreshed by its host.
                        ServerEvent::ResyncRequired { .. } => resnapshot = Some(false),
                        event => {
                            floor = seq;
                            if relayable(&event) {
                                let record = RelayRecord::RelayEvent {
                                    epoch: epoch.clone(),
                                    seq,
                                    event,
                                };
                                // A record too large for the relay is not
                                // sent: the server would see a gap, so a fresh
                                // snapshot replaces it.
                                let fits = serde_json::to_vec(&record)
                                    .is_ok_and(|bytes| bytes.len() < RELAY_RECORD_MAX);
                                if fits {
                                    write_or_end!(&record);
                                } else {
                                    resnapshot = Some(true);
                                }
                            }
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => resnapshot = Some(true),
                Err(broadcast::error::RecvError::Closed) => finish!(RelayEnd::SessionEnded),
            },
        }
        // Something too large to share entered the session's state (an ask or a
        // plan): sharing ends rather than show the phone less than there is.
        if !feed.shareable() {
            finish!(RelayEnd::TooLarge);
        }
        let Some(lost_here) = resnapshot else {
            continue;
        };
        live = feed.live();
        changes.mark_unchanged();
        let taken = match snapshot_now(&feed) {
            Ok(taken) => taken,
            Err(end) => finish!(end),
        };
        // A loss counts whether this reader saw it or the feed rebuilt after
        // one; a refresh by the host alone never does.
        let lost = lost_here || taken.losses > seen_losses;
        seen_losses = taken.losses;
        if lost {
            let now = Instant::now();
            resnapshots.retain(|at| now.duration_since(*at) < RESNAPSHOT_WINDOW);
            resnapshots.push(now);
            if resnapshots.len() > RESNAPSHOT_LIMIT {
                finish!(RelayEnd::Unstable);
            }
        }
        (epoch, floor) = taken.position;
        for record in &taken.records {
            write_or_end!(record);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_harness::RuntimeEvent;
    use localpilot_rpc::{AskRegistry, ServerRecord};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use crate::relay_source::SessionSource;

    fn identity() -> RelayIdentity {
        RelayIdentity {
            session: SessionId::new(),
            owner_generation: 1,
            holder_instance: "me".to_string(),
            share: "s".to_string(),
        }
    }

    const QUICK: Duration = Duration::from_secs(3);

    /// A server that reads the registration, answers ready, reads `lines`
    /// more records, and then stops reading for good.
    async fn stalling_server(server: tokio::io::DuplexStream, lines: usize) {
        let (read, mut write) = tokio::io::split(server);
        let mut read = BufReader::new(read);
        let mut line = String::new();
        let _ = read.read_line(&mut line).await;
        let ready = ServerRecord {
            v: RPC_PROTOCOL_VERSION,
            id: None,
            seq: None,
            event: ServerEvent::RelayReady {
                relay: "r".to_string(),
            },
        };
        let mut bytes = serde_json::to_vec(&ready).unwrap();
        bytes.push(b'\n');
        let _ = write.write_all(&bytes).await;
        for _ in 0..lines {
            line.clear();
            let _ = read.read_line(&mut line).await;
        }
        // Keep the connection open without reading.
        std::future::pending::<()>().await;
    }

    async fn relay_over(
        pipe: usize,
        source: &SessionSource,
        stop: &CancellationToken,
        server: impl FnOnce(tokio::io::DuplexStream) -> tokio::task::JoinHandle<()>,
    ) -> tokio::task::JoinHandle<std::io::Result<RelayEnd>> {
        let (client, peer) = tokio::io::duplex(pipe);
        let _server = server(peer);
        let feed = source.feed();
        let stop = stop.clone();
        tokio::spawn(async move { run_relay(client, feed, &identity(), &stop, None, None).await })
    }

    #[tokio::test]
    async fn stopping_ends_a_relay_whose_registration_write_is_stuck() {
        let source = SessionSource::start(AskRegistry::default());
        let stop = CancellationToken::new();
        // A peer that never reads at all, through a pipe too small for the
        // registration record.
        let relay = relay_over(8, &source, &stop, |peer| {
            tokio::spawn(async move {
                let _keep = peer;
                std::future::pending::<()>().await;
            })
        })
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        stop.cancel();
        let end = tokio::time::timeout(QUICK, relay)
            .await
            .expect("ended")
            .unwrap();
        assert_eq!(end.unwrap(), RelayEnd::Stopped);
    }

    #[tokio::test]
    async fn stopping_ends_a_relay_whose_snapshot_write_is_stuck() {
        let source = SessionSource::start(AskRegistry::default());
        let _ = source
            .turn_events()
            .send(RuntimeEvent::Text("x".repeat(4096)));
        tokio::time::sleep(Duration::from_millis(50)).await;
        let stop = CancellationToken::new();
        let relay = relay_over(256, &source, &stop, |peer| {
            tokio::spawn(stalling_server(peer, 0))
        })
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        stop.cancel();
        let end = tokio::time::timeout(QUICK, relay)
            .await
            .expect("ended")
            .unwrap();
        assert_eq!(end.unwrap(), RelayEnd::Stopped);
    }

    #[tokio::test]
    async fn the_session_ending_ends_a_relay_whose_delta_write_is_stuck() {
        let source = SessionSource::start(AskRegistry::default());
        let stop = CancellationToken::new();
        // The peer reads the whole (small) snapshot, then stops reading.
        let relay = relay_over(256, &source, &stop, |peer| {
            tokio::spawn(stalling_server(peer, 3))
        })
        .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let events = source.turn_events();
        for _ in 0..50 {
            let _ = events.send(RuntimeEvent::Text("y".repeat(1024)));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(events);
        drop(source); // the session ends: its feed closes
        let end = tokio::time::timeout(QUICK, relay)
            .await
            .expect("ended")
            .unwrap();
        assert_eq!(end.unwrap(), RelayEnd::SessionEnded);
    }

    #[tokio::test]
    async fn a_ready_that_arrives_after_stop_withdraws_the_registration() {
        let source = SessionSource::start(AskRegistry::default());
        let stop = CancellationToken::new();
        let (client, peer) = tokio::io::duplex(64 * 1024);
        let stop_now = stop.clone();
        let server = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(peer);
            let mut read = BufReader::new(read);
            let mut line = String::new();
            let _ = read.read_line(&mut line).await;
            // Sharing is stopped while the server is still answering.
            stop_now.cancel();
            let ready = ServerRecord {
                v: RPC_PROTOCOL_VERSION,
                id: None,
                seq: None,
                event: ServerEvent::RelayReady {
                    relay: "r".to_string(),
                },
            };
            let mut bytes = serde_json::to_vec(&ready).unwrap();
            bytes.push(b'\n');
            let _ = write.write_all(&bytes).await;
            let mut rest = String::new();
            while read.read_line(&mut rest).await.is_ok_and(|n| n > 0) {}
            rest
        });
        let end = run_relay(client, source.feed(), &identity(), &stop, None, None)
            .await
            .unwrap();
        assert_eq!(end, RelayEnd::Stopped);
        let rest = tokio::time::timeout(QUICK, server).await.unwrap().unwrap();
        assert!(
            !rest.contains("relay_snapshot"),
            "nothing was published: {rest}"
        );
    }

    #[tokio::test]
    async fn a_command_for_an_earlier_share_or_another_relay_never_runs() {
        let source = SessionSource::start(AskRegistry::default());
        let stop = CancellationToken::new();
        let (client, peer) = tokio::io::duplex(64 * 1024);
        let (requests, mut incoming) = tokio::sync::mpsc::channel(4);
        let host = SourceHost {
            requests,
            standing: Arc::new(|_| Some(localpilot_rpc::Scope::Control)),
            open: Arc::new(|| true),
            attendance: None,
            asks: AskRegistry::default(),
        };
        let relay = tokio::spawn({
            let feed = source.feed();
            let stop = stop.clone();
            async move { run_relay(client, feed, &identity(), &stop, Some(host), None).await }
        });
        let (read, mut write) = tokio::io::split(peer);
        let mut read = BufReader::new(read);
        let mut line = String::new();
        read.read_line(&mut line).await.unwrap(); // relay_host
        let send = |event: ServerEvent| {
            let mut bytes = serde_json::to_vec(&ServerRecord {
                v: RPC_PROTOCOL_VERSION,
                id: None,
                seq: None,
                event,
            })
            .unwrap();
            bytes.push(b'\n');
            bytes
        };
        write
            .write_all(&send(ServerEvent::RelayReady {
                relay: "r1".to_string(),
            }))
            .await
            .unwrap();
        let command = |relay: &str, share: &str, n: u64| ServerEvent::RelayCommand {
            relay: relay.to_string(),
            share: share.to_string(),
            device: "phone".to_string(),
            n,
            command: Box::new(ClientCommand::Prompt {
                text: "go".to_string(),
                disposition: Default::default(),
            }),
        };
        write.write_all(&send(command("r0", "s", 1))).await.unwrap();
        write
            .write_all(&send(command("r1", "old-share", 2)))
            .await
            .unwrap();
        // Read until both results arrive.
        let mut unknown = 0;
        while unknown < 2 {
            line.clear();
            read.read_line(&mut line).await.unwrap();
            if line.contains("relay_result") {
                assert!(line.contains("\"state\":\"unknown\""), "{line}");
                unknown += 1;
            }
        }
        assert!(incoming.try_recv().is_err(), "nothing reached the host");
        stop.cancel();
        let _ = tokio::time::timeout(QUICK, relay).await;
    }

    fn shown(text: &str) -> crate::relay_source::History {
        crate::relay_source::history_of(&[localpilot_core::Message::text(
            localpilot_core::Role::Assistant,
            text,
        )])
    }

    #[tokio::test]
    async fn refreshes_by_the_host_never_end_sharing() {
        let source = SessionSource::start(AskRegistry::default());
        let stop = CancellationToken::new();
        let relay = relay_over(1 << 20, &source, &stop, |peer| {
            tokio::spawn(stalling_server(peer, usize::MAX))
        })
        .await;
        for n in 0..6 {
            source.refresh(shown(&format!("cleared {n}")));
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        assert!(
            !relay.is_finished(),
            "six refreshes in a minute keep sharing"
        );
        stop.cancel();
        let end = tokio::time::timeout(QUICK, relay).await.unwrap().unwrap();
        assert_eq!(end.unwrap(), RelayEnd::Stopped);
    }

    #[tokio::test]
    async fn losses_end_sharing_even_when_each_races_a_refresh() {
        let source = SessionSource::start(AskRegistry::default());
        let stop = CancellationToken::new();
        let relay = relay_over(1 << 20, &source, &stop, |peer| {
            tokio::spawn(stalling_server(peer, usize::MAX))
        })
        .await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        for n in 0..4 {
            source.refresh(shown(&format!("cleared {n}")));
            source.lose();
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        let end = tokio::time::timeout(QUICK, relay).await.unwrap().unwrap();
        assert_eq!(end.unwrap(), RelayEnd::Unstable);
    }
}
