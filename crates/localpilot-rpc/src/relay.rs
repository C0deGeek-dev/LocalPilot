//! Records of a relay connection: a full-screen session published to the
//! workspace server so paired devices can follow it.
//!
//! The session's own process (the source) opens a local connection whose
//! first record is [`ClientCommand::RelayHost`](crate::ClientCommand::RelayHost).
//! After the server answers [`ServerEvent::RelayReady`], the source sends
//! [`RelayRecord`]s: a snapshot of its session feed at one position, in parts,
//! then every later feed record in order. The server keeps a mirror of that
//! feed for devices and never opens the session itself.
//!
//! A snapshot travels as its JSON text split into parts small enough that the
//! complete encoded part record stays within [`RELAY_RECORD_MAX`] whatever the
//! escaping, with the total bounded by [`RELAY_SNAPSHOT_MAX`] and a SHA-256
//! digest checked before anything is installed.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::protocol::ServerEvent;

/// The largest complete encoded relay record, envelope and escaping included.
pub const RELAY_RECORD_MAX: usize = 256 * 1024;

/// The largest snapshot (its JSON text) a relay carries.
pub const RELAY_SNAPSHOT_MAX: usize = 2 * 1024 * 1024;

/// Snapshot text per part. JSON escaping expands a byte to at most six
/// (`\u00XX`), so a part record stays under [`RELAY_RECORD_MAX`].
const PART_TEXT: usize = 40 * 1024;

/// The most parts a snapshot may be announced in: enough for
/// [`RELAY_SNAPSHOT_MAX`] in full-size parts, with room for character
/// boundaries.
pub const RELAY_SNAPSHOT_PARTS_MAX: u32 = 64;

/// One record from the source on a relay connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RelayRecord {
    /// A snapshot of the source feed at `seq` in `epoch` follows in `parts`
    /// parts of `bytes` bytes of snapshot text in all.
    RelaySnapshotBegin {
        epoch: String,
        seq: u64,
        parts: u32,
        bytes: u64,
    },
    /// Part `index` (from 0) of the snapshot text.
    RelaySnapshotPart { index: u32, data: String },
    /// The snapshot is complete; `digest` is the hex SHA-256 of its text.
    RelaySnapshotCommit { digest: String },
    /// A feed record after the snapshot, in the source's order.
    RelayEvent {
        epoch: String,
        seq: u64,
        event: ServerEvent,
    },
    /// The source stops sharing.
    RelayStop,
    /// The state of command `n` from `device`, as the source decided it.
    /// Sent for every transition (a queued prompt later becomes accepted or
    /// rejected); a repeated request gets its recorded state again.
    RelayResult {
        device: String,
        n: u64,
        state: RelayRequestState,
    },
}

/// What the source made of a forwarded command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RelayRequestState {
    /// Refused; nothing ran.
    Rejected { reason: String },
    /// Accepted as a follow-up behind the running turn.
    Queued,
    /// Accepted and running (a prompt's turn started).
    Accepted,
    /// Finished; `output` is a command's short result, if any.
    Done {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
    /// The source no longer knows this request (it was evicted from its
    /// window, or belongs to an earlier relay); it never runs now.
    Unknown,
}

impl RelayRequestState {
    /// Whether nothing more will happen to the request.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Queued)
    }
}

/// Why a relay record or snapshot was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RelayError {
    #[error("the session is too large to share right now")]
    SnapshotTooLarge,
    #[error("the snapshot arrived out of order")]
    OutOfOrder,
    #[error("the snapshot does not match its digest")]
    BadDigest,
    #[error("the snapshot is not a session snapshot")]
    NotSnapshot,
    #[error("a relay record was too large")]
    RecordTooLarge,
}

/// Whether a feed record is part of what a relay mirrors. Records that bind a
/// connection or belong to one client (attach, pairing, command results,
/// listings) are never relayed.
#[must_use]
pub fn relayable(event: &ServerEvent) -> bool {
    matches!(
        event,
        ServerEvent::TurnStarted
            | ServerEvent::TextDelta { .. }
            | ServerEvent::ReasoningDelta { .. }
            | ServerEvent::ToolStarted { .. }
            | ServerEvent::ToolFinished { .. }
            | ServerEvent::Usage { .. }
            | ServerEvent::ContextUsage { .. }
            | ServerEvent::Compacted { .. }
            | ServerEvent::Warning { .. }
            | ServerEvent::Plan { .. }
            | ServerEvent::QuotaPaused { .. }
            | ServerEvent::Recovery { .. }
            | ServerEvent::Stopped { .. }
            | ServerEvent::PermissionAsk { .. }
            | ServerEvent::PermissionResolved { .. }
            | ServerEvent::ResyncRequired { .. }
    )
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Split a feed snapshot into the records that carry it.
///
/// # Errors
/// [`RelayError::NotSnapshot`] for anything but a snapshot event, and
/// [`RelayError::SnapshotTooLarge`] when its text exceeds
/// [`RELAY_SNAPSHOT_MAX`]; nothing is sent then, and no part of the snapshot
/// is dropped to make it fit.
pub fn snapshot_records(snapshot: &ServerEvent) -> Result<Vec<RelayRecord>, RelayError> {
    let ServerEvent::Snapshot { epoch, seq, .. } = snapshot else {
        return Err(RelayError::NotSnapshot);
    };
    let text = serde_json::to_string(snapshot).map_err(|_| RelayError::NotSnapshot)?;
    if text.len() > RELAY_SNAPSHOT_MAX {
        return Err(RelayError::SnapshotTooLarge);
    }
    let mut parts = Vec::new();
    let mut rest = text.as_str();
    while !rest.is_empty() {
        let mut cut = rest.len().min(PART_TEXT);
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        let (part, tail) = rest.split_at(cut);
        parts.push(part.to_string());
        rest = tail;
    }
    let count = u32::try_from(parts.len()).map_err(|_| RelayError::SnapshotTooLarge)?;
    let mut records = Vec::with_capacity(parts.len() + 2);
    records.push(RelayRecord::RelaySnapshotBegin {
        epoch: epoch.clone(),
        seq: *seq,
        parts: count,
        bytes: text.len() as u64,
    });
    for (index, data) in parts.into_iter().enumerate() {
        records.push(RelayRecord::RelaySnapshotPart {
            index: u32::try_from(index).map_err(|_| RelayError::SnapshotTooLarge)?,
            data,
        });
    }
    records.push(RelayRecord::RelaySnapshotCommit {
        digest: hex_digest(text.as_bytes()),
    });
    Ok(records)
}

/// A snapshot being received. Nothing it holds is usable until
/// [`SnapshotAssembly::commit`] has checked it whole.
#[derive(Debug)]
pub struct SnapshotAssembly {
    epoch: String,
    seq: u64,
    parts: u32,
    bytes: u64,
    next: u32,
    text: String,
}

/// A complete, verified snapshot from the source.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceSnapshot {
    /// The source feed's epoch and position the snapshot was taken at.
    pub epoch: String,
    pub seq: u64,
    /// The snapshot event itself.
    pub snapshot: ServerEvent,
}

impl SnapshotAssembly {
    /// Start receiving a snapshot announced by a begin record.
    ///
    /// # Errors
    /// [`RelayError::SnapshotTooLarge`] when the announced size exceeds the cap.
    pub fn begin(epoch: String, seq: u64, parts: u32, bytes: u64) -> Result<Self, RelayError> {
        // Every part carries at least one byte, and no more parts than a
        // full-size snapshot needs.
        if bytes > RELAY_SNAPSHOT_MAX as u64
            || parts == 0
            || parts > RELAY_SNAPSHOT_PARTS_MAX
            || u64::from(parts) > bytes
        {
            return Err(RelayError::SnapshotTooLarge);
        }
        Ok(Self {
            epoch,
            seq,
            parts,
            bytes,
            next: 0,
            text: String::new(),
        })
    }

    /// Add the next part.
    ///
    /// # Errors
    /// [`RelayError::OutOfOrder`] for a part out of sequence or beyond the
    /// announced count, [`RelayError::SnapshotTooLarge`] past the announced
    /// size.
    pub fn part(&mut self, index: u32, data: &str) -> Result<(), RelayError> {
        if index != self.next || index >= self.parts || data.is_empty() {
            return Err(RelayError::OutOfOrder);
        }
        if (self.text.len() + data.len()) as u64 > self.bytes {
            return Err(RelayError::SnapshotTooLarge);
        }
        self.text.push_str(data);
        self.next += 1;
        Ok(())
    }

    /// Check the whole snapshot against its count, size and digest and parse
    /// it.
    ///
    /// # Errors
    /// [`RelayError::OutOfOrder`] when parts are missing,
    /// [`RelayError::BadDigest`] for a size or digest mismatch, and
    /// [`RelayError::NotSnapshot`] when the text is not a snapshot event.
    pub fn commit(self, digest: &str) -> Result<SourceSnapshot, RelayError> {
        if self.next != self.parts {
            return Err(RelayError::OutOfOrder);
        }
        if self.text.len() as u64 != self.bytes || hex_digest(self.text.as_bytes()) != digest {
            return Err(RelayError::BadDigest);
        }
        let snapshot: ServerEvent =
            serde_json::from_str(&self.text).map_err(|_| RelayError::NotSnapshot)?;
        // The snapshot must describe the position its begin record announced.
        match &snapshot {
            ServerEvent::Snapshot { epoch, seq, .. }
                if *epoch == self.epoch && *seq == self.seq => {}
            ServerEvent::Snapshot { .. } => return Err(RelayError::OutOfOrder),
            _ => return Err(RelayError::NotSnapshot),
        }
        Ok(SourceSnapshot {
            epoch: self.epoch,
            seq: self.seq,
            snapshot,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{AskDetail, TurnState};

    fn snapshot(text: &str, asks: usize) -> ServerEvent {
        ServerEvent::Snapshot {
            epoch: "e1".to_string(),
            seq: 7,
            turn: TurnState::Running,
            busy: true,
            queued: 0,
            plan: None,
            asks: (0..asks)
                .map(|n| AskDetail {
                    ask_id: format!("ask-{n}"),
                    tool: "run_shell".to_string(),
                    detail: "\"quoted\"\n".repeat(200),
                    risk: "shell".to_string(),
                    expires_at_ms: 0,
                })
                .collect(),
            turn_text: Some(text.to_string()),
            turn_text_truncated: false,
            history: Vec::new(),
            history_truncated: false,
        }
    }

    fn assemble(records: &[RelayRecord]) -> Result<SourceSnapshot, RelayError> {
        let mut assembly = None;
        for record in records {
            match record {
                RelayRecord::RelaySnapshotBegin {
                    epoch,
                    seq,
                    parts,
                    bytes,
                } => {
                    assembly = Some(SnapshotAssembly::begin(
                        epoch.clone(),
                        *seq,
                        *parts,
                        *bytes,
                    )?)
                }
                RelayRecord::RelaySnapshotPart { index, data } => {
                    assembly
                        .as_mut()
                        .ok_or(RelayError::OutOfOrder)?
                        .part(*index, data)?;
                }
                RelayRecord::RelaySnapshotCommit { digest } => {
                    return assembly
                        .take()
                        .ok_or(RelayError::OutOfOrder)?
                        .commit(digest);
                }
                _ => {}
            }
        }
        Err(RelayError::OutOfOrder)
    }

    #[test]
    fn an_escape_heavy_snapshot_round_trips_and_every_record_fits() {
        // Control characters escape to six bytes each: the worst case.
        let text = "\u{1}\"\\\n".repeat(60_000);
        let original = snapshot(&text, 3);
        let records = snapshot_records(&original).unwrap();
        assert!(records.len() > 3, "split into several parts");
        for record in &records {
            let encoded = serde_json::to_vec(record).unwrap();
            assert!(encoded.len() <= RELAY_RECORD_MAX, "{} bytes", encoded.len());
        }
        let received = assemble(&records).unwrap();
        assert_eq!(received.snapshot, original);
        assert_eq!((received.epoch.as_str(), received.seq), ("e1", 7));
    }

    #[test]
    fn an_oversized_snapshot_is_refused_whole_rather_than_trimmed() {
        let original = snapshot(&"x".repeat(RELAY_SNAPSHOT_MAX + 1), 1);
        assert_eq!(
            snapshot_records(&original),
            Err(RelayError::SnapshotTooLarge)
        );
    }

    #[test]
    fn a_tampered_missing_or_reordered_snapshot_never_installs() {
        let records = snapshot_records(&snapshot(&"y".repeat(100_000), 1)).unwrap();
        // Tampered part.
        let mut tampered = records.clone();
        if let RelayRecord::RelaySnapshotPart { data, .. } = &mut tampered[1] {
            data.replace_range(0..1, "z");
        }
        assert_eq!(assemble(&tampered), Err(RelayError::BadDigest));
        // Missing part.
        let mut missing = records.clone();
        missing.remove(2);
        assert_eq!(assemble(&missing), Err(RelayError::OutOfOrder));
        // Reordered parts.
        let mut reordered = records.clone();
        reordered.swap(1, 2);
        assert_eq!(assemble(&reordered), Err(RelayError::OutOfOrder));
        // An announced size over the cap is refused before any part.
        assert_eq!(
            SnapshotAssembly::begin("e".to_string(), 1, 1, RELAY_SNAPSHOT_MAX as u64 + 1)
                .unwrap_err(),
            RelayError::SnapshotTooLarge
        );
        // So are more parts than the cap, or than there are bytes, and an
        // empty part.
        assert_eq!(
            SnapshotAssembly::begin("e".to_string(), 1, RELAY_SNAPSHOT_PARTS_MAX + 1, 1 << 20)
                .unwrap_err(),
            RelayError::SnapshotTooLarge
        );
        assert_eq!(
            SnapshotAssembly::begin("e".to_string(), 1, 10, 5).unwrap_err(),
            RelayError::SnapshotTooLarge
        );
        let mut assembly = SnapshotAssembly::begin("e".to_string(), 1, 2, 10).unwrap();
        assert_eq!(assembly.part(0, ""), Err(RelayError::OutOfOrder));
    }

    #[test]
    fn a_snapshot_whose_own_cursor_differs_from_its_announcement_never_installs() {
        let mut records = snapshot_records(&snapshot("t", 0)).unwrap();
        if let RelayRecord::RelaySnapshotBegin { seq, .. } = &mut records[0] {
            *seq += 1;
        }
        assert_eq!(assemble(&records), Err(RelayError::OutOfOrder));
        let mut records = snapshot_records(&snapshot("t", 0)).unwrap();
        if let RelayRecord::RelaySnapshotBegin { epoch, .. } = &mut records[0] {
            *epoch = "other".to_string();
        }
        assert_eq!(assemble(&records), Err(RelayError::OutOfOrder));
    }
}
