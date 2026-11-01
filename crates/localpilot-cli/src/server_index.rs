//! The per-user index of served workspaces, `~/.localpilot/served`.
//!
//! Each `localpilot serve` with `[remote] enabled` publishes one entry while
//! it runs, so the remote gateway can list the user's workspaces and route a
//! device to the one it picks. An entry names the workspace by an opaque
//! discovery id (a hash of its canonical root, never the path) and carries no
//! address: the gateway derives each server's endpoint from the root itself.
//!
//! Entries are written and removed under one stable lock file, atomically, and
//! readable by the owner only. Only the server that published an entry removes
//! it, and only while the entry is still its own, so a server restarting in the
//! same workspace never loses its entry to the previous run's exit.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::server_devices::{prepare_dir, random_hex, restrict};

/// At most this many entries are read; the rest are ignored.
const MAX_ENTRIES: usize = 64;
/// An entry file larger than this is ignored.
const MAX_ENTRY_BYTES: u64 = 8 * 1024;

/// The opaque, stable id a workspace is discovered by: the first 16 hex digits
/// of the SHA-256 of its canonical root. `None` when the root cannot be
/// canonicalized; such a workspace is neither published nor routed to.
pub(crate) fn discovery_id(root: &Path) -> Option<String> {
    let canonical = std::fs::canonicalize(root).ok()?;
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    Some(digest[..8].iter().fold(String::new(), |mut hex, b| {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
        hex
    }))
}

/// Whether `id` looks like a discovery id: exactly 16 lowercase hex digits.
pub(crate) fn is_discovery_id(id: &str) -> bool {
    id.len() == 16
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// One served workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ServedEntry {
    /// The discovery id.
    pub(crate) id: String,
    /// The workspace folder's own name, for display.
    pub(crate) label: String,
    /// The canonical root (never sent to a device).
    pub(crate) root: PathBuf,
    /// Which run of `serve` published the entry.
    instance: String,
}

/// The index directory.
#[derive(Debug, Clone)]
pub(crate) struct ServedIndex {
    dir: PathBuf,
}

/// The per-user index directory, beside the device registry.
fn user_index_dir() -> Option<PathBuf> {
    crate::server_devices::user_devices_dir()
        .and_then(|devices| devices.parent().map(|parent| parent.join("served")))
}

impl ServedIndex {
    pub(crate) fn open(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    /// This user's index, when there is a home directory.
    pub(crate) fn for_user() -> Option<Self> {
        user_index_dir().map(|dir| Self::open(&dir))
    }

    /// Run `work` under the index's stable write lock.
    fn locked<T>(&self, work: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
        prepare_dir(&self.dir)?;
        let lock_path = self.dir.join("served.lock");
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)?;
        restrict(&lock_path)?;
        let mut lock = fd_lock::RwLock::new(lock_file);
        let _held = lock.write()?;
        work()
    }

    /// Publish `root` for as long as the returned guard lives.
    pub(crate) fn publish(&self, root: &Path) -> std::io::Result<Published> {
        let canonical = std::fs::canonicalize(root)?;
        let id = discovery_id(&canonical)
            .ok_or_else(|| std::io::Error::other("the workspace root cannot be canonicalized"))?;
        let label = canonical
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| canonical.to_string_lossy().into_owned());
        let instance = random_hex(8).map_err(|error| std::io::Error::other(error.to_string()))?;
        let entry = ServedEntry {
            id: id.clone(),
            label,
            root: canonical,
            instance: instance.clone(),
        };
        self.locked(|| {
            let bytes = serde_json::to_vec_pretty(&entry).map_err(std::io::Error::other)?;
            let temp = self
                .dir
                .join(format!("{id}.json.{}.tmp", std::process::id()));
            {
                let mut out = std::fs::File::create(&temp)?;
                restrict(&temp)?;
                out.write_all(&bytes)?;
                out.sync_all()?;
            }
            replace(&temp, &self.dir.join(format!("{id}.json")))
        })?;
        Ok(Published {
            index: self.clone(),
            id,
            instance,
        })
    }

    /// Remove the entry for `id` if it is still the one `instance` published.
    fn withdraw(&self, id: &str, instance: &str) -> std::io::Result<()> {
        self.locked(|| {
            let path = self.dir.join(format!("{id}.json"));
            match read_entry(&path) {
                Some(entry) if entry.instance == instance => std::fs::remove_file(&path),
                _ => Ok(()),
            }
        })
    }

    /// The published workspaces: well-formed entries whose id matches both
    /// their file name and their root, at most [`MAX_ENTRIES`].
    pub(crate) fn list(&self) -> Vec<ServedEntry> {
        let Ok(dir) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut entries = Vec::new();
        for item in dir.flatten().take(MAX_ENTRIES * 4) {
            let path = item.path();
            let Some(stem) = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".json"))
            else {
                continue;
            };
            if !is_discovery_id(stem) {
                continue;
            }
            let Some(entry) = read_entry(&path) else {
                continue;
            };
            if entry.id == stem && discovery_id(&entry.root).as_deref() == Some(stem) {
                entries.push(entry);
            }
            if entries.len() == MAX_ENTRIES {
                break;
            }
        }
        entries.sort_by(|a, b| a.label.cmp(&b.label).then_with(|| a.id.cmp(&b.id)));
        entries
    }

    /// The entry for `id`, if it is published.
    #[cfg(test)]
    pub(crate) fn get(&self, id: &str) -> Option<ServedEntry> {
        if !is_discovery_id(id) {
            return None;
        }
        let entry = read_entry(&self.dir.join(format!("{id}.json")))?;
        (entry.id == id && discovery_id(&entry.root).as_deref() == Some(id)).then_some(entry)
    }
}

/// Read one bounded entry file; anything unreadable or malformed is `None`.
fn read_entry(path: &Path) -> Option<ServedEntry> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_ENTRY_BYTES {
        return None;
    }
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Atomically replace `target` with `temp`, retrying while Windows briefly
/// refuses because a reader holds the target open.
fn replace(temp: &Path, target: &Path) -> std::io::Result<()> {
    let mut renamed = std::fs::rename(temp, target);
    for _ in 0..200 {
        match &renamed {
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                std::thread::sleep(std::time::Duration::from_millis(5));
                renamed = std::fs::rename(temp, target);
            }
            _ => break,
        }
    }
    if renamed.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    renamed
}

/// A published entry; dropping it withdraws the entry if it is still ours.
pub(crate) struct Published {
    index: ServedIndex,
    id: String,
    instance: String,
}

impl Published {
    #[cfg(test)]
    pub(crate) fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for Published {
    fn drop(&mut self) {
        let _ = self.index.withdraw(&self.id, &self.instance);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_discovery_id_is_opaque_and_stable() {
        let dir = tempfile::tempdir().unwrap();
        let id = discovery_id(dir.path()).unwrap();
        assert!(is_discovery_id(&id), "{id}");
        assert_eq!(discovery_id(dir.path()), Some(id.clone()));
        let path = dir.path().to_string_lossy().to_string();
        assert!(!path.contains(&id) && !id.contains(std::path::MAIN_SEPARATOR));
        assert_eq!(discovery_id(&dir.path().join("missing")), None);
        for bad in [
            "",
            "../x",
            "ABCDEF0123456789",
            "0123456789abcdeg",
            "0123456789abcdef0",
        ] {
            assert!(!is_discovery_id(bad), "{bad}");
        }
    }

    #[test]
    fn workspaces_with_unusual_names_are_listed_and_withdrawn() {
        let home = tempfile::tempdir().unwrap();
        let index = ServedIndex::open(&home.path().join("served"));
        let a = home.path().join("my project & co");
        let b = home.path().join("Ünïcødé ✓");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let published_a = index.publish(&a).unwrap();
        let published_b = index.publish(&b).unwrap();
        let listed = index.list();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().any(|entry| entry.label == "my project & co"));
        assert!(listed.iter().any(|entry| entry.label == "Ünïcødé ✓"));
        assert_eq!(
            index.get(published_a.id()).unwrap().label,
            "my project & co"
        );
        drop(published_a);
        assert_eq!(index.list().len(), 1);
        drop(published_b);
        assert!(index.list().is_empty());
    }

    #[test]
    fn a_restarted_server_keeps_its_entry_when_the_old_run_exits() {
        let home = tempfile::tempdir().unwrap();
        let index = ServedIndex::open(&home.path().join("served"));
        let root = home.path().join("w");
        std::fs::create_dir_all(&root).unwrap();
        let old = index.publish(&root).unwrap();
        // A new run replaces the entry before the old one finishes exiting.
        let new = index.publish(&root).unwrap();
        drop(old);
        assert_eq!(index.list().len(), 1, "the new run's entry stays");
        drop(new);
        assert!(index.list().is_empty());
    }

    #[test]
    fn entries_that_do_not_match_their_name_or_root_are_ignored() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("served");
        let index = ServedIndex::open(&dir);
        let a = home.path().join("a");
        let b = home.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let published = index.publish(&a).unwrap();
        let id_b = discovery_id(&b).unwrap();
        // An entry claiming b's id but pointing at a's root is not b.
        let forged = std::fs::read_to_string(dir.join(format!("{}.json", published.id())))
            .unwrap()
            .replace(published.id(), &id_b);
        std::fs::write(dir.join(format!("{id_b}.json")), forged).unwrap();
        std::fs::write(dir.join("not-an-id.json"), "{}").unwrap();
        std::fs::write(dir.join("0123456789abcdef.json"), "x".repeat(16 * 1024)).unwrap();
        let listed = index.list();
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].id, published.id());
        assert!(index.get(&id_b).is_none());
        assert!(index.get("../../etc").is_none());
    }
}
