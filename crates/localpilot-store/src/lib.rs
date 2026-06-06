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

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use localpilot_config::redact::redact;
use localpilot_core::{Message, SessionId};

pub use atomic::atomic_write;
pub use error::StoreError;

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
    use localpilot_core::{ContentBlock, Role};

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path());
        (dir, store)
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
}
