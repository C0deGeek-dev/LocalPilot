//! Session persistence for LocalPilot.
//!
//! The store owns the project-local `.localpilot/` directory: transcripts (one
//! JSON message per line), a session index, a file-backed cache, tool-output
//! snapshots, and persisted provider metadata. Everything is an inspectable
//! plain file, written atomically (temp-then-rename), and redacted *before* it
//! touches disk using the workspace's shared secret detector. Export bundles are
//! redacted again on the way out.
#![forbid(unsafe_code)]

mod atomic;
mod error;
mod events;

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use localpilot_config::redact::redact;
use localpilot_core::{ContentBlock, EventId, Message, SessionId};
use serde::{Deserialize, Serialize};

pub use atomic::atomic_write;
pub use error::StoreError;
pub use events::{
    origin_for, transcript_from_events, MessageOrigin, OpenReason, SessionEvent, SessionEventKind,
    SESSION_EVENT_FORMAT_VERSION,
};

const SESSIONS_DIR: &str = "sessions";
const CACHE_DIR: &str = "cache";
const TOOL_OUTPUT_DIR: &str = "tool-output";
const PROVIDERS_DIR: &str = "providers";
const INDEX_FILE: &str = "index.json";

/// A handle to a workspace's `.localpilot/` state directory.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

/// One entry in the session index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIndexEntry {
    pub id: SessionId,
    pub message_count: usize,
    pub created_unix: u64,
    pub updated_unix: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SessionIndex {
    sessions: Vec<SessionIndexEntry>,
}

/// An exported, inspectable session bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionBundle {
    pub id: SessionId,
    pub messages: Vec<Message>,
}

/// How much session history to retain. `0` on either axis means unbounded for
/// that axis; a session is kept only when it satisfies *both* constraints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Keep at most this many of the most-recently-updated sessions.
    pub max_sessions: u64,
    /// Drop sessions not updated within this many days.
    pub max_age_days: u64,
}

impl RetentionPolicy {
    /// Whether this policy can ever remove anything.
    #[must_use]
    pub fn is_unbounded(&self) -> bool {
        self.max_sessions == 0 && self.max_age_days == 0
    }
}

/// What a [`Store::prune`] call removed (or, in a dry run, would remove).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub sessions_removed: usize,
    pub tool_outputs_removed: usize,
}

impl Store {
    /// Open the store under `workspace_root/.localpilot`.
    #[must_use]
    pub fn open(workspace_root: &Path) -> Self {
        Self {
            root: workspace_root.join(".localpilot"),
        }
    }

    /// Open the store at an explicit `.localpilot` directory.
    #[must_use]
    pub fn at(localpilot_dir: PathBuf) -> Self {
        Self {
            root: localpilot_dir,
        }
    }

    /// The state directory root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    // --- transcripts -------------------------------------------------------

    fn session_path(&self, session: SessionId) -> PathBuf {
        self.root
            .join(SESSIONS_DIR)
            .join(format!("{session}.jsonl"))
    }

    /// Append one message to a session transcript, redacting it first and
    /// updating the session index.
    ///
    /// # Errors
    /// Returns [`StoreError`] on serialization or filesystem failure.
    pub fn append_message(&self, session: SessionId, message: &Message) -> Result<(), StoreError> {
        let path = self.session_path(session);
        let mut content = read_to_string_opt(&path)?.unwrap_or_default();

        let line = redact(&serde_json::to_string(message)?);
        content.push_str(&line);
        content.push('\n');
        atomic_write(&path, content.as_bytes())?;

        let count = content.lines().filter(|l| !l.trim().is_empty()).count();
        self.touch_index(session, count)?;
        Ok(())
    }

    /// Read a session transcript back into messages.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a line is not valid JSON or the file cannot be
    /// read. A missing session yields an empty transcript.
    pub fn read_transcript(&self, session: SessionId) -> Result<Vec<Message>, StoreError> {
        let path = self.session_path(session);
        let Some(content) = read_to_string_opt(&path)? else {
            return Ok(Vec::new());
        };
        let mut messages = Vec::new();
        for line in content.lines().filter(|l| !l.trim().is_empty()) {
            messages.push(serde_json::from_str(line)?);
        }
        Ok(messages)
    }

    // --- session event log ---------------------------------------------------

    fn events_path(&self, session: SessionId) -> PathBuf {
        self.root
            .join(SESSIONS_DIR)
            .join(format!("{session}.events.jsonl"))
    }

    /// Append one event to a session's durable event log, redacting it first.
    /// Returns the event's id for parent chaining.
    ///
    /// # Errors
    /// Returns [`StoreError`] on serialization or filesystem failure.
    pub fn append_event(
        &self,
        session: SessionId,
        parent: Option<EventId>,
        kind: SessionEventKind,
    ) -> Result<EventId, StoreError> {
        let event = SessionEvent {
            v: SESSION_EVENT_FORMAT_VERSION,
            id: EventId::new(),
            parent_id: parent,
            at_unix: now_unix(),
            kind,
        };
        let path = self.events_path(session);
        let mut content = read_to_string_opt(&path)?.unwrap_or_default();
        content.push_str(&redact(&serde_json::to_string(&event)?));
        content.push('\n');
        atomic_write(&path, content.as_bytes())?;
        Ok(event.id)
    }

    /// Read a session's event log in order, migrating older format versions on
    /// load. A missing log yields an empty sequence.
    ///
    /// # Errors
    /// Returns [`StoreError`] if a line is unreadable or written by a newer
    /// format version than this build supports.
    pub fn read_events(&self, session: SessionId) -> Result<Vec<SessionEvent>, StoreError> {
        let Some(content) = read_to_string_opt(&self.events_path(session))? else {
            return Ok(Vec::new());
        };
        content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(SessionEvent::from_line)
            .collect()
    }

    // --- index -------------------------------------------------------------

    fn index_path(&self) -> PathBuf {
        self.root.join(INDEX_FILE)
    }

    /// List indexed sessions.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the index exists but cannot be read or parsed.
    pub fn list_sessions(&self) -> Result<Vec<SessionIndexEntry>, StoreError> {
        Ok(self.load_index()?.sessions)
    }

    /// The most recently updated session in this workspace, if any.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the index exists but cannot be read or parsed.
    pub fn latest_session(&self) -> Result<Option<SessionIndexEntry>, StoreError> {
        Ok(self
            .load_index()?
            .sessions
            .into_iter()
            .max_by_key(|entry| entry.updated_unix))
    }

    fn load_index(&self) -> Result<SessionIndex, StoreError> {
        match read_to_string_opt(&self.index_path())? {
            Some(content) if !content.trim().is_empty() => Ok(serde_json::from_str(&content)?),
            _ => Ok(SessionIndex::default()),
        }
    }

    fn touch_index(&self, session: SessionId, message_count: usize) -> Result<(), StoreError> {
        let mut index = self.load_index()?;
        let now = now_unix();
        if let Some(entry) = index.sessions.iter_mut().find(|e| e.id == session) {
            entry.message_count = message_count;
            entry.updated_unix = now;
        } else {
            index.sessions.push(SessionIndexEntry {
                id: session,
                message_count,
                created_unix: now,
                updated_unix: now,
            });
        }
        atomic_write(
            &self.index_path(),
            serde_json::to_string_pretty(&index)?.as_bytes(),
        )
    }

    // --- cache -------------------------------------------------------------

    /// Store raw bytes in the file-backed cache under `key`.
    ///
    /// # Errors
    /// Returns [`StoreError::InvalidKey`] for an unsafe key, or an io error.
    pub fn put_cache(&self, key: &str, value: &[u8]) -> Result<(), StoreError> {
        let path = self.root.join(CACHE_DIR).join(safe_key(key)?);
        atomic_write(&path, value)
    }

    /// Read cached bytes for `key`, or `None` if absent.
    ///
    /// # Errors
    /// Returns [`StoreError::InvalidKey`] for an unsafe key, or an io error.
    pub fn get_cache(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let path = self.root.join(CACHE_DIR).join(safe_key(key)?);
        read_bytes_opt(&path)
    }

    /// Remove a cached entry. A no-op if the key is absent.
    ///
    /// # Errors
    /// Returns [`StoreError::InvalidKey`] for an unsafe key, or an io error.
    pub fn delete_cache(&self, key: &str) -> Result<(), StoreError> {
        let path = self.root.join(CACHE_DIR).join(safe_key(key)?);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StoreError::io(&path, source)),
        }
    }

    // --- tool-output snapshots --------------------------------------------

    /// Persist a redacted tool-output snapshot keyed by `id`.
    ///
    /// # Errors
    /// Returns [`StoreError::InvalidKey`] for an unsafe id, or an io error.
    pub fn put_tool_output(&self, id: &str, output: &str) -> Result<(), StoreError> {
        let path = self
            .root
            .join(TOOL_OUTPUT_DIR)
            .join(format!("{}.txt", safe_key(id)?));
        atomic_write(&path, redact(output).as_bytes())
    }

    /// Read a tool-output snapshot, or `None` if absent.
    ///
    /// # Errors
    /// Returns [`StoreError::InvalidKey`] for an unsafe id, or an io error.
    pub fn get_tool_output(&self, id: &str) -> Result<Option<String>, StoreError> {
        let path = self
            .root
            .join(TOOL_OUTPUT_DIR)
            .join(format!("{}.txt", safe_key(id)?));
        read_to_string_opt(&path)
    }

    // --- provider metadata -------------------------------------------------

    /// Persist redacted provider metadata keyed by `provider_id`.
    ///
    /// # Errors
    /// Returns [`StoreError::InvalidKey`] for an unsafe id, or a serde/io error.
    pub fn put_provider_metadata(
        &self,
        provider_id: &str,
        metadata: &serde_json::Value,
    ) -> Result<(), StoreError> {
        let path = self
            .root
            .join(PROVIDERS_DIR)
            .join(format!("{}.json", safe_key(provider_id)?));
        let redacted = redact(&serde_json::to_string_pretty(metadata)?);
        atomic_write(&path, redacted.as_bytes())
    }

    /// Read provider metadata, or `None` if absent.
    ///
    /// # Errors
    /// Returns [`StoreError::InvalidKey`] for an unsafe id, or a serde/io error.
    pub fn get_provider_metadata(
        &self,
        provider_id: &str,
    ) -> Result<Option<serde_json::Value>, StoreError> {
        let path = self
            .root
            .join(PROVIDERS_DIR)
            .join(format!("{}.json", safe_key(provider_id)?));
        match read_to_string_opt(&path)? {
            Some(content) => Ok(Some(serde_json::from_str(&content)?)),
            None => Ok(None),
        }
    }

    // --- export ------------------------------------------------------------

    /// Export a session as an inspectable, redacted bundle written atomically to
    /// `destination`.
    ///
    /// # Errors
    /// Returns [`StoreError`] on read, serialization, or write failure.
    pub fn export_session(&self, session: SessionId, destination: &Path) -> Result<(), StoreError> {
        let bundle = SessionBundle {
            id: session,
            messages: self.read_transcript(session)?,
        };
        // The transcript is already redacted at rest; redact again so the export
        // path is safe regardless of how the bundle was assembled.
        let redacted = redact(&serde_json::to_string_pretty(&bundle)?);
        atomic_write(destination, redacted.as_bytes())
    }

    // --- retention ---------------------------------------------------------

    /// Apply a [`RetentionPolicy`]: remove the session transcripts and event logs
    /// that fall outside it, prune the index, and sweep any tool-output snapshot
    /// no surviving session still references. With `dry_run`, nothing is deleted
    /// and the report describes what *would* be removed.
    ///
    /// # Errors
    /// Returns [`StoreError`] if the index or a transcript cannot be read, or a
    /// delete/index write fails.
    pub fn prune(
        &self,
        policy: RetentionPolicy,
        now_unix: u64,
        dry_run: bool,
    ) -> Result<PruneReport, StoreError> {
        let index = self.load_index()?;
        let doomed = sessions_to_remove(&index.sessions, policy, now_unix);

        // The keys (file stems under `tool-output/`) still owned by survivors:
        // a `recovery-<id>` snapshot per session, plus every tool-use id that
        // appears in its transcript. Anything else is an orphan to sweep.
        let mut live_keys: HashSet<String> = HashSet::new();
        for entry in &index.sessions {
            if doomed.contains(&entry.id) {
                continue;
            }
            live_keys.insert(format!("recovery-{}", entry.id));
            for message in self.read_transcript(entry.id)? {
                collect_tool_output_keys(&message, &mut live_keys);
            }
        }

        let mut tool_outputs_removed = 0;
        for stem in self.tool_output_stems()? {
            if !live_keys.contains(&stem) {
                tool_outputs_removed += 1;
                if !dry_run {
                    self.remove_tool_output(&stem)?;
                }
            }
        }

        if !dry_run && !doomed.is_empty() {
            for id in &doomed {
                remove_file_if_present(&self.session_path(*id))?;
                remove_file_if_present(&self.events_path(*id))?;
            }
            let kept = SessionIndex {
                sessions: index
                    .sessions
                    .into_iter()
                    .filter(|e| !doomed.contains(&e.id))
                    .collect(),
            };
            atomic_write(
                &self.index_path(),
                serde_json::to_string_pretty(&kept)?.as_bytes(),
            )?;
        }

        Ok(PruneReport {
            sessions_removed: doomed.len(),
            tool_outputs_removed,
        })
    }

    /// The file stems (names without the `.txt` suffix) present under
    /// `tool-output/`. A missing directory yields an empty list.
    fn tool_output_stems(&self) -> Result<Vec<String>, StoreError> {
        let dir = self.root.join(TOOL_OUTPUT_DIR);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(StoreError::io(&dir, e)),
        };
        let mut stems = Vec::new();
        for entry in entries {
            let path = entry.map_err(|e| StoreError::io(&dir, e))?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("txt") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    stems.push(stem.to_string());
                }
            }
        }
        Ok(stems)
    }

    fn remove_tool_output(&self, stem: &str) -> Result<(), StoreError> {
        remove_file_if_present(&self.root.join(TOOL_OUTPUT_DIR).join(format!("{stem}.txt")))
    }
}

/// Which sessions a policy removes: those that fall outside the most-recent
/// `max_sessions` window, or were last updated before the `max_age_days` cutoff.
/// A `0` on either axis disables that constraint.
fn sessions_to_remove(
    entries: &[SessionIndexEntry],
    policy: RetentionPolicy,
    now_unix: u64,
) -> HashSet<SessionId> {
    let mut ordered: Vec<&SessionIndexEntry> = entries.iter().collect();
    // Most recently updated first; ties broken by id for a stable order.
    ordered.sort_by(|a, b| {
        b.updated_unix
            .cmp(&a.updated_unix)
            .then_with(|| a.id.to_string().cmp(&b.id.to_string()))
    });

    let age_cutoff = (policy.max_age_days > 0)
        .then(|| now_unix.saturating_sub(policy.max_age_days.saturating_mul(86_400)));

    ordered
        .into_iter()
        .enumerate()
        .filter(|(rank, entry)| {
            let over_count = policy.max_sessions > 0 && *rank as u64 >= policy.max_sessions;
            let too_old = age_cutoff.is_some_and(|cutoff| entry.updated_unix < cutoff);
            over_count || too_old
        })
        .map(|(_, entry)| entry.id)
        .collect()
}

/// Add the tool-output snapshot keys referenced by one message: the ids of any
/// tool calls and tool results it carries.
fn collect_tool_output_keys(message: &Message, keys: &mut HashSet<String>) {
    for block in &message.content {
        match block {
            ContentBlock::ToolUse(call) => {
                keys.insert(call.id.to_string());
            }
            ContentBlock::ToolResult(result) => {
                keys.insert(result.id.to_string());
            }
            _ => {}
        }
    }
}

fn remove_file_if_present(path: &Path) -> Result<(), StoreError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(StoreError::io(path, source)),
    }
}

fn read_to_string_opt(path: &Path) -> Result<Option<String>, StoreError> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(StoreError::io(path, e)),
    }
}

fn read_bytes_opt(path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
    match fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(StoreError::io(path, e)),
    }
}

/// Accept only file-name-safe keys so a key can never escape its directory.
fn safe_key(key: &str) -> Result<String, StoreError> {
    if !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && key != "."
        && key != ".."
    {
        Ok(key.to_string())
    } else {
        Err(StoreError::InvalidKey(key.to_string()))
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_core::{ContentBlock, Role, ToolCall, ToolUseId};

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path());
        (dir, store)
    }

    fn entry(updated_unix: u64) -> SessionIndexEntry {
        SessionIndexEntry {
            id: SessionId::new(),
            message_count: 1,
            created_unix: updated_unix,
            updated_unix,
        }
    }

    #[test]
    fn transcript_write_read_roundtrip() {
        let (_dir, store) = store();
        let session = SessionId::new();
        let a = Message::text(Role::User, "hello");
        let b = Message::text(Role::Assistant, "hi there");
        store.append_message(session, &a).unwrap();
        store.append_message(session, &b).unwrap();

        let read = store.read_transcript(session).unwrap();
        assert_eq!(read, vec![a, b]);

        let sessions = store.list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].message_count, 2);
    }

    #[test]
    fn interrupted_write_leaves_no_corrupt_session() {
        let (_dir, store) = store();
        let session = SessionId::new();
        store
            .append_message(session, &Message::text(Role::User, "committed"))
            .unwrap();

        // A leftover temp file (a crash before rename) must not corrupt reads.
        let path = store.session_path(session);
        let mut tmp = path.file_name().unwrap().to_os_string();
        tmp.push(".tmp");
        std::fs::write(path.with_file_name(tmp), b"{ partial").unwrap();

        let read = store.read_transcript(session).unwrap();
        assert_eq!(read.len(), 1);
    }

    #[test]
    fn redaction_is_applied_before_persistence() {
        let (_dir, store) = store();
        let session = SessionId::new();
        let secret = "sk-abcdefghijklmnopqrstuvwxyz0123";
        store
            .append_message(
                session,
                &Message::new(
                    Role::User,
                    vec![ContentBlock::text(format!("key={secret}"))],
                ),
            )
            .unwrap();

        let raw = std::fs::read_to_string(store.session_path(session)).unwrap();
        assert!(!raw.contains(secret), "secret reached disk: {raw}");
        assert!(raw.contains("[REDACTED]"));
    }

    #[test]
    fn cache_tool_output_and_provider_metadata_roundtrip_and_redact() {
        let (_dir, store) = store();

        store.put_cache("models.json", b"[\"a\",\"b\"]").unwrap();
        assert_eq!(
            store.get_cache("models.json").unwrap().unwrap(),
            b"[\"a\",\"b\"]"
        );
        assert!(store.get_cache("missing").unwrap().is_none());

        store
            .put_tool_output("call_1", "Bearer abcdef123456ghijkl token")
            .unwrap();
        let out = store.get_tool_output("call_1").unwrap().unwrap();
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains("abcdef123456ghijkl"));

        store
            .put_provider_metadata("openai", &serde_json::json!({ "limit": "tier1" }))
            .unwrap();
        let meta = store.get_provider_metadata("openai").unwrap().unwrap();
        assert_eq!(meta["limit"], "tier1");
    }

    #[test]
    fn unsafe_keys_are_rejected() {
        let (_dir, store) = store();
        assert!(matches!(
            store.put_cache("../escape", b"x"),
            Err(StoreError::InvalidKey(_))
        ));
        assert!(matches!(
            store.get_tool_output("a/b"),
            Err(StoreError::InvalidKey(_))
        ));
    }

    #[test]
    fn export_writes_redacted_bundle() {
        let (dir, store) = store();
        let session = SessionId::new();
        store
            .append_message(session, &Message::text(Role::User, "hello"))
            .unwrap();
        let dest = dir.path().join("export.json");
        store.export_session(session, &dest).unwrap();

        let bundle: SessionBundle =
            serde_json::from_str(&std::fs::read_to_string(&dest).unwrap()).unwrap();
        assert_eq!(bundle.id, session);
        assert_eq!(bundle.messages.len(), 1);
    }

    #[test]
    fn sessions_to_remove_honors_count_age_and_unbounded() {
        // Newest to oldest: e3 (300), e2 (200), e1 (100).
        let e1 = entry(100);
        let e2 = entry(200);
        let e3 = entry(300);
        let entries = vec![e1.clone(), e2.clone(), e3.clone()];

        // Count only: keep the 2 newest, drop the oldest.
        let by_count = sessions_to_remove(
            &entries,
            RetentionPolicy {
                max_sessions: 2,
                max_age_days: 0,
            },
            10_000,
        );
        assert_eq!(by_count, HashSet::from([e1.id]));

        // Age only: cutoff = now - 1 day = 300. Sessions updated before 300 go;
        // e3 at exactly the cutoff stays.
        let by_age = sessions_to_remove(
            &entries,
            RetentionPolicy {
                max_sessions: 0,
                max_age_days: 1,
            },
            300 + 86_400,
        );
        assert_eq!(by_age, HashSet::from([e1.id, e2.id]));

        // Unbounded removes nothing.
        let none = sessions_to_remove(
            &entries,
            RetentionPolicy {
                max_sessions: 0,
                max_age_days: 0,
            },
            10_000,
        );
        assert!(none.is_empty());
    }

    /// Re-stamp the index so `newest` is more recently updated than every other
    /// session, making the prune order deterministic regardless of wall-clock
    /// timing.
    fn restamp(store: &Store, newest: SessionId) {
        let mut index = store.load_index().unwrap();
        for e in &mut index.sessions {
            e.updated_unix = if e.id == newest { 200 } else { 100 };
        }
        atomic_write(
            &store.index_path(),
            serde_json::to_string_pretty(&index).unwrap().as_bytes(),
        )
        .unwrap();
    }

    #[test]
    fn prune_drops_old_sessions_and_sweeps_orphan_tool_output() {
        let (_dir, store) = store();
        let keep = SessionId::new();
        let doomed = SessionId::new();

        // The surviving session references tool output `callKeep`; the doomed one
        // references `callDrop`; `orphan` belongs to nobody.
        store
            .append_message(
                keep,
                &Message::new(
                    Role::Assistant,
                    vec![ContentBlock::ToolUse(ToolCall::new(
                        ToolUseId::from("callKeep"),
                        "run",
                        serde_json::json!({}),
                    ))],
                ),
            )
            .unwrap();
        store
            .append_message(
                doomed,
                &Message::new(
                    Role::Assistant,
                    vec![ContentBlock::ToolUse(ToolCall::new(
                        ToolUseId::from("callDrop"),
                        "run",
                        serde_json::json!({}),
                    ))],
                ),
            )
            .unwrap();
        store.put_tool_output("callKeep", "out").unwrap();
        store.put_tool_output("callDrop", "out").unwrap();
        store.put_tool_output("orphan", "out").unwrap();
        restamp(&store, keep);

        let report = store
            .prune(
                RetentionPolicy {
                    max_sessions: 1,
                    max_age_days: 0,
                },
                0,
                false,
            )
            .unwrap();
        assert_eq!(report.sessions_removed, 1);
        assert_eq!(report.tool_outputs_removed, 2); // callDrop + orphan

        // Survivor intact; doomed session and orphaned outputs gone.
        assert!(store.session_path(keep).exists());
        assert!(!store.session_path(doomed).exists());
        assert!(store.get_tool_output("callKeep").unwrap().is_some());
        assert!(store.get_tool_output("callDrop").unwrap().is_none());
        assert!(store.get_tool_output("orphan").unwrap().is_none());
        assert_eq!(store.list_sessions().unwrap().len(), 1);
    }

    #[test]
    fn prune_dry_run_reports_without_deleting() {
        let (_dir, store) = store();
        let keep = SessionId::new();
        let doomed = SessionId::new();
        store
            .append_message(keep, &Message::text(Role::User, "a"))
            .unwrap();
        store
            .append_message(doomed, &Message::text(Role::User, "b"))
            .unwrap();
        store.put_tool_output("orphan", "out").unwrap();
        restamp(&store, keep);

        let report = store
            .prune(
                RetentionPolicy {
                    max_sessions: 1,
                    max_age_days: 0,
                },
                0,
                true,
            )
            .unwrap();
        assert_eq!(report.sessions_removed, 1);
        assert_eq!(report.tool_outputs_removed, 1);

        // Nothing was actually removed.
        assert!(store.session_path(doomed).exists());
        assert!(store.get_tool_output("orphan").unwrap().is_some());
        assert_eq!(store.list_sessions().unwrap().len(), 2);
    }
}
