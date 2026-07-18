//! Durable prompt history for the interactive composer.
//!
//! A single global append-only JSONL file under the per-user directory holds every
//! submitted prompt, each record tagged with the directory it was typed in and a
//! timestamp. Recall seeds from this store at session start, filtered to the
//! current project, so Up/Down survives a restart while staying relevant to the
//! repo. The store is deliberately separate from the project-local session store:
//! one global file is simpler to manage and keeps cross-project recall reachable.
//!
//! Unlike transcripts, history text is stored **raw**, not redacted — a history
//! entry exists only to be recalled verbatim into the composer, so redacting it
//! would recall a placeholder and defeat the feature. The privacy controls are
//! instead the opt-out, the restrictive file mode (0600 on unix), the per-user
//! location, and the bounded on-disk cap.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::atomic::atomic_write;
use crate::error::StoreError;

/// On-disk record format version, carried so a future migration can recognise
/// older lines. Bump only on a breaking shape change.
pub const HISTORY_FORMAT_VERSION: u32 = 1;

/// Most records kept on disk. Older lines are trimmed on write so the global file
/// cannot grow without bound across a long-lived install. Generous relative to the
/// in-session recall cap so several projects' histories survive together.
const MAX_HISTORY_ENTRIES: usize = 1_000;

/// A collapsed paste carried with a history entry: the placeholder shown in
/// the prompt text and the content it stands for. Persisted so a recalled
/// prompt can be re-expanded instead of sending the placeholder verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryPaste {
    /// The placeholder as it appears in the entry's `text`.
    pub placeholder: String,
    /// The pasted content the placeholder stands for.
    pub content: String,
}

/// The most paste content (total bytes across one entry's pastes) persisted
/// with a history entry. Beyond this the entry is stored without its mappings
/// — recall then replays the placeholder, exactly the pre-mapping behaviour —
/// so a single monster paste cannot balloon the global history file, which is
/// re-read and rewritten on every submit.
const PASTE_PERSIST_BUDGET: usize = 64 * 1024;

/// One persisted prompt: the visible text, the directory it was submitted in, and
/// when. Stored raw (no redaction) so recall is faithful.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Record format version (see [`HISTORY_FORMAT_VERSION`]).
    pub v: u32,
    /// The visible prompt text, exactly as the user submitted it.
    pub text: String,
    /// The working directory the prompt was submitted in (recall filter key).
    pub cwd: String,
    /// Unix submission time, seconds since the epoch.
    pub at_unix: u64,
    /// Paste mappings for placeholders in `text`. `#[serde(default)]` so lines
    /// written before pastes were carried still load; omitted when empty so
    /// paste-free entries keep their old shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pastes: Vec<HistoryPaste>,
}

impl HistoryEntry {
    fn new(text: String, pastes: Vec<HistoryPaste>, cwd: &Path) -> Self {
        Self {
            v: HISTORY_FORMAT_VERSION,
            text,
            cwd: cwd_key(cwd),
            at_unix: crate::now_unix(),
            pastes,
        }
    }
}

/// A handle to the global prompt-history store. Constructed disabled (the opt-out)
/// or pointed at the per-user file; every operation no-ops when disabled.
#[derive(Debug, Clone)]
pub struct PromptHistory {
    /// Whether persistence is active. `false` is the `persistence = "none"`
    /// opt-out: no read, no write, no file created.
    enabled: bool,
    /// The store file location, or `None` when the per-user dir cannot be resolved.
    path: Option<PathBuf>,
}

impl PromptHistory {
    /// A store honouring the opt-out, resolving the per-user file location. When
    /// `enabled` is `false` every operation is a no-op.
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            path: localpilot_config::prompt_history_path(),
        }
    }

    /// A store over an explicit file (or `None` to disable), for tests and callers
    /// that resolve their own path.
    #[must_use]
    pub fn with_store(path: Option<PathBuf>) -> Self {
        Self {
            enabled: path.is_some(),
            path,
        }
    }

    /// Whether reads and writes are active.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled && self.path.is_some()
    }

    /// Load the store, newest entry last, capped to [`MAX_HISTORY_ENTRIES`].
    /// Tolerant: a missing file, an unreadable file, or a partial/corrupt line
    /// never errors — it yields what parses, so a session never fails to start
    /// because history will not load. Returns empty when disabled.
    #[must_use]
    pub fn load(&self) -> Vec<HistoryEntry> {
        if !self.enabled {
            return Vec::new();
        }
        match &self.path {
            Some(path) => read_entries(path),
            None => Vec::new(),
        }
    }

    /// Append one prompt with the paste mappings its placeholders depend on,
    /// tagged with `cwd` and the current time, trimming the on-disk file to
    /// the cap and applying the restrictive mode. A no-op when disabled or
    /// when the prompt is blank or repeats the last entry. Mappings beyond
    /// [`PASTE_PERSIST_BUDGET`] are dropped (the entry itself is still kept).
    ///
    /// # Errors
    /// Returns [`StoreError::NoUserDir`] when persistence is enabled but the
    /// per-user directory cannot be resolved, or an io/serde error on write.
    pub fn append(
        &self,
        text: &str,
        pastes: &[HistoryPaste],
        cwd: &Path,
    ) -> Result<(), StoreError> {
        if !self.enabled || text.trim().is_empty() {
            return Ok(());
        }
        let path = self.path.as_ref().ok_or(StoreError::NoUserDir)?;

        let mut entries = read_entries(path);
        // Match the in-session recall behaviour: never record a consecutive
        // duplicate of the most recent prompt.
        if entries.last().is_some_and(|last| last.text == text) {
            return Ok(());
        }
        let paste_bytes: usize = pastes.iter().map(|paste| paste.content.len()).sum();
        let kept_pastes = if paste_bytes <= PASTE_PERSIST_BUDGET {
            pastes.to_vec()
        } else {
            Vec::new()
        };
        entries.push(HistoryEntry::new(text.to_string(), kept_pastes, cwd));

        let start = entries.len().saturating_sub(MAX_HISTORY_ENTRIES);
        let mut body = String::new();
        for entry in &entries[start..] {
            body.push_str(&serde_json::to_string(entry)?);
            body.push('\n');
        }
        atomic_write(path, body.as_bytes())?;
        harden_perms(path)
    }
}

/// The recalled prompts submitted in `cwd` (oldest first), for project-scoped
/// seeding of the composer's history.
#[must_use]
pub fn project_texts(entries: &[HistoryEntry], cwd: &Path) -> Vec<String> {
    project_entries(entries, cwd)
        .into_iter()
        .map(|entry| entry.text)
        .collect()
}

/// Every recalled prompt (oldest first), regardless of project, for the
/// view-all-projects toggle.
#[must_use]
pub fn all_texts(entries: &[HistoryEntry]) -> Vec<String> {
    entries.iter().map(|entry| entry.text.clone()).collect()
}

/// The full entries submitted in `cwd` (oldest first) — text plus paste
/// mappings — for hosts that rehydrate pastes on recall.
#[must_use]
pub fn project_entries(entries: &[HistoryEntry], cwd: &Path) -> Vec<HistoryEntry> {
    let key = cwd_key(cwd);
    entries
        .iter()
        .filter(|entry| entry.cwd == key)
        .cloned()
        .collect()
}

/// The directory tag for a path: its lossy string form. Write and filter use the
/// same key so an exact match scopes recall to a project without canonicalising
/// (which could fail or follow symlinks differently between runs).
fn cwd_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Read and parse the store, skipping blank or unparseable lines, capped to the
/// most recent [`MAX_HISTORY_ENTRIES`]. Any read failure yields an empty list.
fn read_entries(path: &Path) -> Vec<HistoryEntry> {
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut entries: Vec<HistoryEntry> = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<HistoryEntry>(line).ok())
        .collect();
    let start = entries.len().saturating_sub(MAX_HISTORY_ENTRIES);
    if start > 0 {
        entries.drain(0..start);
    }
    entries
}

/// Restrict the store file to owner read/write on unix. On other platforms the
/// per-user profile directory's own ACL is the protection (tier-1 parity is
/// behaviour parity; the FS permission mechanism differs by platform).
#[cfg(unix)]
fn harden_perms(path: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|e| StoreError::io(path, e))
}

#[cfg(not(unix))]
fn harden_perms(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_at(dir: &tempfile::TempDir) -> PromptHistory {
        PromptHistory::with_store(Some(dir.path().join("prompt-history.jsonl")))
    }

    #[test]
    fn append_then_load_round_trips_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir);
        let cwd = Path::new("/work/project-a");
        store.append("first", &[], cwd).unwrap();
        store.append("second", &[], cwd).unwrap();

        let entries = store.load();
        assert_eq!(all_texts(&entries), vec!["first", "second"]);
        assert!(entries.iter().all(|e| e.v == HISTORY_FORMAT_VERSION));
        assert!(entries.iter().all(|e| e.cwd == "/work/project-a"));
    }

    #[test]
    fn project_filter_scopes_recall_to_one_cwd_while_all_keeps_everything() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir);
        let a = Path::new("/work/project-a");
        let b = Path::new("/work/project-b");
        store.append("a-one", &[], a).unwrap();
        store.append("b-one", &[], b).unwrap();
        store.append("a-two", &[], a).unwrap();

        let entries = store.load();
        // Project recall sees only its own cwd's prompts, in order.
        assert_eq!(project_texts(&entries, a), vec!["a-one", "a-two"]);
        assert_eq!(project_texts(&entries, b), vec!["b-one"]);
        // View-all exposes the entries the project filter excluded.
        assert_eq!(all_texts(&entries), vec!["a-one", "b-one", "a-two"]);
    }

    #[test]
    fn a_consecutive_duplicate_is_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir);
        let cwd = Path::new("/work/p");
        store.append("same", &[], cwd).unwrap();
        store.append("same", &[], cwd).unwrap();
        store.append("other", &[], cwd).unwrap();
        store.append("same", &[], cwd).unwrap();
        // The immediate repeat is dropped; a later non-adjacent repeat is kept.
        assert_eq!(all_texts(&store.load()), vec!["same", "other", "same"]);
    }

    #[test]
    fn blank_prompts_are_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir);
        let cwd = Path::new("/work/p");
        store.append("   \n  ", &[], cwd).unwrap();
        assert!(store.load().is_empty());
        assert!(!dir.path().join("prompt-history.jsonl").exists());
    }

    #[test]
    fn the_on_disk_file_is_bounded_to_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir);
        let cwd = Path::new("/work/p");
        for i in 0..(MAX_HISTORY_ENTRIES + 50) {
            store.append(&format!("prompt-{i}"), &[], cwd).unwrap();
        }
        let entries = store.load();
        assert_eq!(entries.len(), MAX_HISTORY_ENTRIES);
        // The oldest were trimmed; the newest survive.
        assert_eq!(entries.first().unwrap().text, "prompt-50");
        assert_eq!(
            entries.last().unwrap().text,
            format!("prompt-{}", MAX_HISTORY_ENTRIES + 49)
        );
    }

    #[test]
    fn a_truncated_or_corrupt_line_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prompt-history.jsonl");
        let good = serde_json::to_string(&HistoryEntry::new(
            "kept".to_string(),
            Vec::new(),
            Path::new("/p"),
        ))
        .unwrap();
        // A valid line, a partial JSON line (crash mid-append), and junk.
        fs::write(
            &path,
            format!("{good}\n{{\"v\":1,\"text\":\"part\nnot json\n"),
        )
        .unwrap();
        let store = PromptHistory::with_store(Some(path));
        assert_eq!(all_texts(&store.load()), vec!["kept"]);
    }

    #[test]
    fn paste_mappings_round_trip_with_their_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir);
        let cwd = Path::new("/work/p");
        let pastes = vec![HistoryPaste {
            placeholder: "[3 pasted rows #1]".to_string(),
            content: "a\nb\nc".to_string(),
        }];
        store
            .append("check this: [3 pasted rows #1]", &pastes, cwd)
            .unwrap();

        let entries = store.load();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].pastes, pastes);
    }

    #[test]
    fn an_over_budget_paste_keeps_the_entry_but_drops_the_mappings() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir);
        let cwd = Path::new("/work/p");
        let huge = vec![HistoryPaste {
            placeholder: "[1 pasted rows #1]".to_string(),
            content: "x".repeat(PASTE_PERSIST_BUDGET + 1),
        }];
        store.append("[1 pasted rows #1]", &huge, cwd).unwrap();

        let entries = store.load();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "[1 pasted rows #1]");
        assert!(entries[0].pastes.is_empty(), "mappings beyond budget drop");
    }

    #[test]
    fn a_line_written_before_pastes_were_carried_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prompt-history.jsonl");
        // The exact pre-paste on-disk shape: no `pastes` field at all.
        fs::write(
            &path,
            "{\"v\":1,\"text\":\"old prompt\",\"cwd\":\"/p\",\"at_unix\":1}\n",
        )
        .unwrap();
        let store = PromptHistory::with_store(Some(path));
        let entries = store.load();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "old prompt");
        assert!(entries[0].pastes.is_empty());
    }

    #[test]
    fn project_entries_keeps_mappings_and_scopes_by_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir);
        let a = Path::new("/work/a");
        let b = Path::new("/work/b");
        let paste = vec![HistoryPaste {
            placeholder: "[2 pasted rows #1]".to_string(),
            content: "x\ny".to_string(),
        }];
        store.append("[2 pasted rows #1]", &paste, a).unwrap();
        store.append("plain", &[], b).unwrap();

        let entries = store.load();
        let scoped = project_entries(&entries, a);
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].pastes, paste);
    }

    #[test]
    fn the_opt_out_neither_reads_nor_writes() {
        let dir = tempfile::tempdir().unwrap();
        let candidate = dir.path().join("prompt-history.jsonl");
        // Seed a file the disabled store must not read.
        let seeded = serde_json::to_string(&HistoryEntry::new(
            "secret".to_string(),
            Vec::new(),
            Path::new("/p"),
        ))
        .unwrap();
        fs::write(&candidate, format!("{seeded}\n")).unwrap();

        let off = PromptHistory::with_store(None);
        assert!(!off.is_enabled());
        off.append("nothing", &[], Path::new("/p")).unwrap();
        assert!(off.load().is_empty());

        // `none` constructed the production way also no-ops and reads nothing.
        let off2 = PromptHistory {
            enabled: false,
            path: Some(candidate.clone()),
        };
        off2.append("still nothing", &[], Path::new("/p")).unwrap();
        assert!(off2.load().is_empty());
        // The disabled store never touched the seeded file.
        assert_eq!(
            fs::read_to_string(&candidate).unwrap(),
            format!("{seeded}\n")
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_store_file_is_mode_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir);
        store.append("x", &[], Path::new("/p")).unwrap();
        let mode = fs::metadata(dir.path().join("prompt-history.jsonl"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "history file must be owner-only");
    }
}
