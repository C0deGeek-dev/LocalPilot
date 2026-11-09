//! Paired devices, shared by every workspace's server, and the gateway's
//! capability.
//!
//! One pairing reaches every workspace (or only the ones it was granted): the
//! registry lives once per user, in `~/.localpilot/devices/registry.json`, and
//! each workspace's `localpilot serve` daemon reads it before every decision,
//! so a device paired or revoked from one workspace takes effect in all of
//! them. Writers take an exclusive lock on a separate, never-replaced
//! `registry.lock`, re-read the file, change it, and replace it atomically
//! (temporary file + rename) before the change takes effect. The directory
//! and its files are restricted to the owner. A registry that cannot be read
//! disables device access until it is repaired — it is never replaced by an
//! empty one; local use is unaffected.
//!
//! A device credential is `lpd1.<device id>.<secret>`, the secret being 32
//! random bytes. Only the SHA-256 of the whole credential is stored, compared
//! in constant time. Credentials and pairing secrets live in [`Secret`], which
//! never prints its contents.

use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use localpilot_rpc::Scope;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

/// The credential format's prefix and version.
const CREDENTIAL_PREFIX: &str = "lpd1";
/// Random bytes in a secret.
const SECRET_BYTES: usize = 32;

/// A secret value (credential, capability, pairing code). Its `Debug` is
/// redacted and it has no `Display`; [`expose`](Self::expose) is the one way
/// out, used only to send it to the party it belongs to.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Secret(String);

impl Secret {
    /// The secret's text, for the single reply that must carry it.
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// `n` random bytes from the operating system, as lowercase hex.
pub(crate) fn random_hex(n: usize) -> Result<String, DeviceError> {
    let mut bytes = vec![0u8; n];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| DeviceError::Random)?;
    Ok(bytes
        .iter()
        .fold(String::with_capacity(n * 2), |mut out, b| {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
            out
        }))
}

/// A secret: 32 random bytes as hex.
fn random_token() -> Result<String, DeviceError> {
    random_hex(SECRET_BYTES)
}

fn sha256_hex(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// Compare two digests without timing that depends on where they differ
/// (`subtle`, whose comparisons are built to resist the optimizer). Callers
/// compare fixed-length SHA-256 hex digests, so the length check reveals
/// nothing secret.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq as _;
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

/// Split a presented credential into its device id, without trusting it.
pub(crate) fn credential_device_id(credential: &str) -> Option<&str> {
    let mut parts = credential.splitn(3, '.');
    let (prefix, id, secret) = (parts.next()?, parts.next()?, parts.next()?);
    let hex = |text: &str| {
        text.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    let well_formed = prefix == CREDENTIAL_PREFIX
        && id.len() == 16
        && hex(id)
        && secret.len() == SECRET_BYTES * 2
        && hex(secret);
    well_formed.then_some(id)
}

/// Why a device registry operation failed.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DeviceError {
    #[error("the device registry is unavailable: {0}")]
    Unavailable(String),
    #[error("could not write the device registry: {0}")]
    Persist(String),
    #[error("the operating system's random source failed")]
    Random,
    /// The registry's write lock was not free before the deadline: nothing
    /// was changed.
    #[error("the device registry is busy")]
    Busy,
}

/// At most this many devices are shown at once (the rest are counted).
pub(crate) const ROWS_SHOWN: usize = 256;
/// At most this many of a device's workspaces are shown (the rest counted).
pub(crate) const ROOTS_SHOWN: usize = 16;
/// Display text (names, folders) is cut to this many bytes.
const TEXT_BYTES: usize = 256;

/// `text` for display: control characters made spaces, cut to
/// [`TEXT_BYTES`] on a character boundary (with an ellipsis when cut).
fn bounded_text(text: &str) -> String {
    let clean: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if clean.len() <= TEXT_BYTES {
        return clean;
    }
    let mut end = TEXT_BYTES - '…'.len_utf8();
    while !clean.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &clean[..end])
}

/// Whether `id` can be acted on exactly as written (an id the registry
/// itself makes); anything else is shown but never revoked from the view.
fn actionable_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// A device's grant, as seen from this workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceGrant {
    Active,
    Revoked,
    NotGrantedHere,
}

/// Which workspaces a device may reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeviceReach {
    All,
    /// The first of them, and how many more.
    Roots {
        shown: Vec<String>,
        more: usize,
    },
}

/// One device, as shown: metadata only, bounded, never a credential or hash.
/// `id` is the registry's key for it (bounded for display; exact whenever
/// the row is actionable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceRow {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) scope: Scope,
    pub(crate) grant: DeviceGrant,
    pub(crate) reach: DeviceReach,
    pub(crate) created_unix: u64,
    pub(crate) revoked_unix: Option<u64>,
    /// Whether this row can be revoked from the view: its record's id is
    /// exactly its registry key, and well formed.
    pub(crate) actionable: bool,
}

/// The devices shown, and how many there are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceRows {
    pub(crate) rows: Vec<DeviceRow>,
    pub(crate) total: usize,
    /// Active and revoked records, counted over the whole registry (before
    /// the rows are capped), from the same read.
    pub(crate) active: usize,
    pub(crate) revoked: usize,
}

/// What became of a revoke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RevokeOutcome {
    Revoked(DeviceRow),
    AlreadyRevoked(DeviceRow),
    NotFound,
}

/// One paired device, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DeviceRecord {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) scope: Scope,
    /// SHA-256 (hex) of the full credential.
    credential_sha256: String,
    /// The workspaces (canonical roots) the device may reach; `None` for all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) workspaces: Option<Vec<String>>,
    pub(crate) created_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) revoked_unix: Option<u64>,
    /// The device's phone-notification subscription, while notifications are
    /// on. An additive field: a registry without it reads as "off".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) push: Option<PushSubscription>,
}

/// A browser push subscription, as stored. The endpoint and both keys are
/// secrets to anyone but the browser's push service: `Debug` shows only the
/// endpoint's host, the revision and the key id.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PushSubscription {
    pub(crate) endpoint: String,
    /// The browser's public key (uncompressed P-256, base64url).
    pub(crate) p256dh: String,
    /// The browser's authentication secret (base64url).
    pub(crate) auth: String,
    /// A fresh random value for every subscribe or replace: a queued send or
    /// a cleanup acts only on the exact subscription it was made for.
    pub(crate) rev: String,
    /// The id of the notification key the browser subscribed with.
    pub(crate) key_id: String,
    pub(crate) created_unix: u64,
}

impl PushSubscription {
    /// The endpoint's host, the only part shown or audited.
    pub(crate) fn host(&self) -> String {
        reqwest::Url::parse(&self.endpoint)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
            .unwrap_or_default()
    }
}

impl std::fmt::Debug for PushSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushSubscription")
            .field("host", &self.host())
            .field("rev", &self.rev)
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

/// Why a subscription could not be saved.
#[derive(Debug)]
pub(crate) enum PushSaveError {
    /// The device is unknown or revoked.
    NotAllowed,
    /// The registry could not be read or written.
    Registry,
}

impl DeviceRecord {
    fn reaches(&self, workspace: &str) -> bool {
        self.workspaces
            .as_ref()
            .is_none_or(|granted| granted.iter().any(|root| root == workspace))
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct RegistryFile {
    version: u32,
    devices: BTreeMap<String, DeviceRecord>,
}

impl RegistryFile {
    fn empty() -> Self {
        Self {
            version: 1,
            devices: BTreeMap::new(),
        }
    }
}

/// Why a device was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Device access is disabled (the registry could not be read).
    Unavailable,
    /// No such device, a malformed credential, or the wrong secret.
    BadCredential,
    /// The device was revoked.
    Revoked,
    /// The device was not granted this workspace.
    NotGranted,
}

/// A device's current standing, re-read before each decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Standing {
    pub(crate) id: String,
    pub(crate) scope: Scope,
}

/// One device's record (if it still exists) and whether it may act here
/// now, from one read of the registry.
pub(crate) type DeviceStandingRead = (Option<DeviceRecord>, Result<Scope, Refusal>);

/// What a verified device that is not revoked is told about itself. Read
/// from its record, not from a standing: a standing answers for the
/// registry's own workspace, and the gateway serves every workspace.
pub(crate) fn device_view(
    registry: &DeviceRegistry,
    id: &str,
) -> Option<crate::remote_gateway::DeviceView> {
    let file = registry.current().ok()?;
    let record = file.devices.get(id)?;
    if record.revoked_unix.is_some() {
        return None;
    }
    Some(crate::remote_gateway::DeviceView {
        id: record.id.clone(),
        name: record.name.clone(),
        scope: record.scope,
        this_workspace_only: record.workspaces.is_some(),
        paired_unix: record.created_unix,
    })
}

/// The paired devices of this user. See the module docs.
pub(crate) struct DeviceRegistry {
    /// The registry directory, or `None` for an in-memory registry (tests).
    dir: Option<PathBuf>,
    /// The in-memory registry's contents (unused when `dir` is set).
    memory: Mutex<RegistryFile>,
    /// The in-memory registry's audit lines (unused when `dir` is set).
    audit_memory: Mutex<Vec<String>>,
    /// The canonical root of the workspace this server serves; device grants
    /// are checked against it.
    workspace: String,
    /// Why devices are unavailable regardless of the file (no home directory).
    disabled: Option<String>,
    /// Tests: reading the registry fails while set.
    #[cfg(test)]
    failing: std::sync::atomic::AtomicBool,
    /// Tests: run at points of a bounded change.
    #[cfg(test)]
    hooks: Mutex<HashMap<Hook, AdmittedHook>>,
}

/// Where a test hook runs in a bounded change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Hook {
    /// Before each attempt at the write lock.
    Attempting,
    /// Just after admission (before anything is read or edited).
    Admitted,
    /// Just after the change was written.
    Committed,
}

#[cfg(test)]
pub(crate) type AdmittedHook = std::sync::Arc<dyn Fn() + Send + Sync>;

impl Default for DeviceRegistry {
    fn default() -> Self {
        Self {
            dir: None,
            memory: Mutex::new(RegistryFile::empty()),
            audit_memory: Mutex::new(Vec::new()),
            workspace: String::new(),
            disabled: None,
            #[cfg(test)]
            failing: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            hooks: Mutex::new(HashMap::new()),
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The canonical form of a workspace root, as grants record it.
pub(crate) fn workspace_id(root: &Path) -> String {
    std::fs::canonicalize(root)
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// The per-user registry directory, `~/.localpilot/devices`.
pub(crate) fn user_devices_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .map(|home| home.join(".localpilot").join("devices"))
}

/// Read the registry for a decision. A missing file is an empty registry only
/// if the registry was never written; see [`read_registry_from`].
fn read_registry(dir: &Path) -> Result<RegistryFile, String> {
    read_registry_from(dir, !dir.join("registry.lock").exists())
}

/// Read the registry. `missing_is_empty` is true when nothing can be replacing
/// it right now: before its first write, or for a writer holding the lock.
fn read_registry_from(dir: &Path, missing_is_empty: bool) -> Result<RegistryFile, String> {
    // While another server replaces the file, Windows can briefly refuse to
    // open it or not find it. Both are retried. A registry that has ever been
    // written (its lock file exists) and still cannot be found is an error,
    // never an empty registry.
    let path = dir.join("registry.json");
    let written_before = !missing_is_empty;
    let transient = |error: &std::io::Error| {
        error.kind() == std::io::ErrorKind::PermissionDenied
            || (written_before && error.kind() == std::io::ErrorKind::NotFound)
    };
    let mut read = std::fs::read(&path);
    for _ in 0..50 {
        match &read {
            Err(error) if transient(error) => {
                std::thread::sleep(std::time::Duration::from_millis(5));
                read = std::fs::read(&path);
            }
            _ => break,
        }
    }
    match read {
        Ok(bytes) => serde_json::from_slice::<RegistryFile>(&bytes)
            .map_err(|error| format!("registry.json is not valid: {error}"))
            .and_then(|file| {
                if file.version == 1 {
                    Ok(file)
                } else {
                    Err(format!(
                        "registry.json has unknown version {}",
                        file.version
                    ))
                }
            }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !written_before => {
            Ok(RegistryFile::empty())
        }
        Err(error) => Err(format!("registry.json cannot be read: {error}")),
    }
}

impl DeviceRegistry {
    /// The registry in `dir`, as seen by the server of `workspace_root`.
    pub(crate) fn open(dir: &Path, workspace_root: &Path) -> Self {
        Self {
            dir: Some(dir.to_path_buf()),
            memory: Mutex::new(RegistryFile::empty()),
            audit_memory: Mutex::new(Vec::new()),
            workspace: workspace_id(workspace_root),
            disabled: None,
            #[cfg(test)]
            failing: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            hooks: Mutex::new(HashMap::new()),
        }
    }

    /// The registry of this user, as seen by the server of `workspace_root`.
    /// Without a home directory there is no per-user registry, and devices are
    /// unavailable: no other location stands in for it.
    pub(crate) fn for_user(workspace_root: &Path) -> Self {
        match user_devices_dir() {
            Some(dir) => Self::open(&dir, workspace_root),
            None => Self {
                disabled: Some(
                    "no home directory is set, so there is no per-user device registry".to_string(),
                ),
                workspace: workspace_id(workspace_root),
                ..Self::default()
            },
        }
    }

    /// The current registry: read from disk each time (it is small, and other
    /// workspaces' servers may have changed it), or the in-memory copy.
    fn current(&self) -> Result<RegistryFile, String> {
        if let Some(reason) = &self.disabled {
            return Err(reason.clone());
        }
        #[cfg(test)]
        if self.failing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("the registry cannot be read (test)".to_string());
        }
        match &self.dir {
            Some(dir) => read_registry(dir),
            None => Ok(self
                .memory
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()),
        }
    }

    /// Why device access is disabled, if it is.
    pub(crate) fn unavailable(&self) -> Option<String> {
        self.current().err()
    }

    fn standing_of(&self, record: &DeviceRecord) -> Result<Standing, Refusal> {
        if record.revoked_unix.is_some() {
            return Err(Refusal::Revoked);
        }
        if !record.reaches(&self.workspace) {
            return Err(Refusal::NotGranted);
        }
        Ok(Standing {
            id: record.id.clone(),
            scope: record.scope,
        })
    }

    /// Verify a presented credential: the device it names exists, is not
    /// revoked, may reach this workspace, and the credential matches. The
    /// result is a current standing, never a cached grant.
    pub(crate) fn verify(&self, credential: &str) -> Result<Standing, Refusal> {
        let file = self.current().map_err(|_| Refusal::Unavailable)?;
        let id = credential_device_id(credential).ok_or(Refusal::BadCredential)?;
        let record = file.devices.get(id).ok_or(Refusal::BadCredential)?;
        let presented = sha256_hex(credential);
        if !constant_time_eq(presented.as_bytes(), record.credential_sha256.as_bytes()) {
            return Err(Refusal::BadCredential);
        }
        self.standing_of(record)
    }

    /// Verify a credential whatever workspace this registry was opened for:
    /// the device exists, is not revoked and the credential matches. Used by
    /// the gateway to decide which workspaces to list; each workspace's grant
    /// is then checked with [`DeviceRegistry::standing_in`].
    pub(crate) fn verify_device(&self, credential: &str) -> Result<String, Refusal> {
        let file = self.current().map_err(|_| Refusal::Unavailable)?;
        let id = credential_device_id(credential).ok_or(Refusal::BadCredential)?;
        let record = file.devices.get(id).ok_or(Refusal::BadCredential)?;
        let presented = sha256_hex(credential);
        if !constant_time_eq(presented.as_bytes(), record.credential_sha256.as_bytes()) {
            return Err(Refusal::BadCredential);
        }
        if record.revoked_unix.is_some() {
            return Err(Refusal::Revoked);
        }
        Ok(record.id.clone())
    }

    /// The device's current standing in the workspace at `root` (rather than
    /// this registry's own workspace).
    pub(crate) fn standing_in(&self, id: &str, root: &Path) -> Result<Standing, Refusal> {
        let file = self.current().map_err(|_| Refusal::Unavailable)?;
        let record = file.devices.get(id).ok_or(Refusal::BadCredential)?;
        if record.revoked_unix.is_some() {
            return Err(Refusal::Revoked);
        }
        if !record.reaches(&workspace_id(root)) {
            return Err(Refusal::NotGranted);
        }
        Ok(Standing {
            id: record.id.clone(),
            scope: record.scope,
        })
    }

    /// The device's current standing by id, re-read before each command.
    pub(crate) fn standing(&self, id: &str) -> Result<Standing, Refusal> {
        let file = self.current().map_err(|_| Refusal::Unavailable)?;
        let record = file.devices.get(id).ok_or(Refusal::BadCredential)?;
        self.standing_of(record)
    }

    /// Every device, revoked ones included, newest first.
    /// The standing of each of `ids` from one read of the registry: its
    /// record (when it still exists) and whether it may act here now (`Err`
    /// says why not). The whole answer is `None` when the registry cannot be
    /// read, so it is never mixed from different versions of the file.
    pub(crate) fn standings(&self, ids: &[String]) -> Option<Vec<DeviceStandingRead>> {
        let file = self.current().ok()?;
        Some(
            ids.iter()
                .map(|id| match file.devices.get(id) {
                    Some(record) => (
                        Some(record.clone()),
                        self.standing_of(record).map(|standing| standing.scope),
                    ),
                    None => (None, Err(Refusal::BadCredential)),
                })
                .collect(),
        )
    }

    /// Tests: change an in-memory record in place.
    #[cfg(test)]
    pub(crate) fn edit_for_test(&self, id: &str, edit: impl FnOnce(&mut DeviceRecord)) {
        let mut memory = self.memory.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(record) = memory.devices.get_mut(id) {
            edit(record);
        }
    }

    /// Tests: remove an in-memory record.
    #[cfg(test)]
    pub(crate) fn forget_for_test(&self, id: &str) {
        self.memory
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .devices
            .remove(id);
    }

    /// Tests: make reading the registry fail (or work again).
    #[cfg(test)]
    pub(crate) fn fail_for_test(&self, failing: bool) {
        self.failing
            .store(failing, std::sync::atomic::Ordering::SeqCst);
    }

    /// Tests: run `hook` just after each bounded change is admitted to the
    /// write lock (before it reads or edits anything).
    #[cfg(test)]
    pub(crate) fn on_admitted_for_test(&self, hook: Option<AdmittedHook>) {
        self.hook_at_for_test(Hook::Admitted, hook);
    }

    /// Tests: run `hook` at `which` point of each bounded change.
    #[cfg(test)]
    pub(crate) fn hook_at_for_test(&self, which: Hook, hook: Option<AdmittedHook>) {
        let mut hooks = self.hooks.lock().unwrap_or_else(PoisonError::into_inner);
        match hook {
            Some(hook) => hooks.insert(which, hook),
            None => hooks.remove(&which),
        };
    }

    /// Every device as metadata for display, newest first, from one read of
    /// the registry: never a credential or its hash. At most [`ROWS_SHOWN`]
    /// rows; `total` says how many there are.
    pub(crate) fn rows(&self) -> Result<DeviceRows, DeviceError> {
        let file = self.current().map_err(DeviceError::Unavailable)?;
        let mut records: Vec<(&String, &DeviceRecord)> = file.devices.iter().collect();
        records.sort_by(|a, b| b.1.created_unix.cmp(&a.1.created_unix));
        let revoked = records
            .iter()
            .filter(|(_, record)| record.revoked_unix.is_some())
            .count();
        Ok(DeviceRows {
            total: records.len(),
            active: records.len() - revoked,
            revoked,
            rows: records
                .into_iter()
                .take(ROWS_SHOWN)
                .map(|(key, record)| self.row_of(key, record))
                .collect(),
        })
    }

    /// A device as shown, keyed by the registry's own key: only a record
    /// whose id is exactly that key (and a well-formed one) can be acted on,
    /// so a revoke from the view always names the device the row shows.
    fn row_of(&self, key: &str, record: &DeviceRecord) -> DeviceRow {
        let grant = if record.revoked_unix.is_some() {
            DeviceGrant::Revoked
        } else if record.reaches(&self.workspace) {
            DeviceGrant::Active
        } else {
            DeviceGrant::NotGrantedHere
        };
        let reach = match &record.workspaces {
            None => DeviceReach::All,
            Some(roots) => DeviceReach::Roots {
                shown: roots
                    .iter()
                    .take(ROOTS_SHOWN)
                    .map(|root| bounded_text(root))
                    .collect(),
                more: roots.len().saturating_sub(ROOTS_SHOWN),
            },
        };
        DeviceRow {
            actionable: record.id == key && actionable_id(key),
            id: bounded_text(key),
            name: bounded_text(&record.name),
            scope: record.scope,
            grant,
            reach,
            created_unix: record.created_unix,
            revoked_unix: record.revoked_unix,
        }
    }

    /// Revoke a device everywhere, saying what happened: decided and
    /// persisted under one admission to the write lock, which must come
    /// before `deadline` (otherwise [`DeviceError::Busy`], and nothing
    /// changed). Once admitted, the change runs to completion.
    pub(crate) fn revoke_device_within(
        &self,
        id: &str,
        deadline: std::time::Instant,
    ) -> Result<RevokeOutcome, DeviceError> {
        let mut outcome = RevokeOutcome::NotFound;
        self.change_within(Some(deadline), |file| {
            if let Some(record) = file.devices.get_mut(id) {
                if record.revoked_unix.is_none() {
                    record.revoked_unix = Some(now_unix());
                    record.push = None;
                    outcome = RevokeOutcome::Revoked(self.row_of(id, record));
                } else {
                    outcome = RevokeOutcome::AlreadyRevoked(self.row_of(id, record));
                }
            }
        })?;
        Ok(outcome)
    }

    /// [`DeviceRegistry::revoke_device_within`], waiting for the lock as long
    /// as it takes (the command line).
    pub(crate) fn revoke_device(&self, id: &str) -> Result<RevokeOutcome, DeviceError> {
        let mut outcome = RevokeOutcome::NotFound;
        self.change_within(None, |file| {
            if let Some(record) = file.devices.get_mut(id) {
                if record.revoked_unix.is_none() {
                    record.revoked_unix = Some(now_unix());
                    record.push = None;
                    outcome = RevokeOutcome::Revoked(self.row_of(id, record));
                } else {
                    outcome = RevokeOutcome::AlreadyRevoked(self.row_of(id, record));
                }
            }
        })?;
        Ok(outcome)
    }

    pub(crate) fn list(&self) -> Result<Vec<DeviceRecord>, DeviceError> {
        let file = self.current().map_err(DeviceError::Unavailable)?;
        let mut devices: Vec<DeviceRecord> = file.devices.values().cloned().collect();
        devices.sort_by(|a, b| b.created_unix.cmp(&a.created_unix));
        Ok(devices)
    }

    /// Register a new device and return its credential. `this_workspace_only`
    /// limits it to this server's workspace; otherwise it reaches every
    /// workspace. Persisted before it takes effect: on a write failure the
    /// registry is unchanged.
    pub(crate) fn add(
        &self,
        name: &str,
        scope: Scope,
        this_workspace_only: bool,
    ) -> Result<(Standing, Secret), DeviceError> {
        let id = random_hex(8)?;
        let credential = format!("{CREDENTIAL_PREFIX}.{id}.{}", random_token()?);
        let record = DeviceRecord {
            id: id.clone(),
            name: name.trim().chars().take(64).collect(),
            scope,
            credential_sha256: sha256_hex(&credential),
            workspaces: this_workspace_only.then(|| vec![self.workspace.clone()]),
            created_unix: now_unix(),
            revoked_unix: None,
            push: None,
        };
        self.change(|file| {
            file.devices.insert(id.clone(), record);
        })?;
        Ok((Standing { id, scope }, Secret(credential)))
    }

    /// Save a device's push subscription (replacing any earlier one) under a
    /// fresh revision, which is returned. Refused for an unknown or revoked
    /// device. The caller holds the notification-key lock and has checked
    /// that `key_id` is the current key (lock order: key, then registry).
    pub(crate) fn set_push(
        &self,
        id: &str,
        endpoint: &str,
        p256dh: &str,
        auth: &str,
        key_id: &str,
    ) -> Result<String, PushSaveError> {
        let rev = random_hex(16).map_err(|_| PushSaveError::Registry)?;
        let mut saved = false;
        self.change(|file| {
            if let Some(record) = file.devices.get_mut(id) {
                if record.revoked_unix.is_none() {
                    record.push = Some(PushSubscription {
                        endpoint: endpoint.to_string(),
                        p256dh: p256dh.to_string(),
                        auth: auth.to_string(),
                        rev: rev.clone(),
                        key_id: key_id.to_string(),
                        created_unix: now_unix(),
                    });
                    saved = true;
                }
            }
        })
        .map_err(|_| PushSaveError::Registry)?;
        if saved {
            Ok(rev)
        } else {
            Err(PushSaveError::NotAllowed)
        }
    }

    /// Turn a device's notifications off. Returns whether one was on.
    pub(crate) fn clear_push(&self, id: &str) -> Result<bool, DeviceError> {
        let mut cleared = false;
        self.change(|file| {
            if let Some(record) = file.devices.get_mut(id) {
                cleared = record.push.take().is_some();
            }
        })?;
        Ok(cleared)
    }

    /// Remove a device's subscription only if it is still the one with
    /// revision `rev` (a push service said it is gone). A replacement made
    /// meanwhile is kept.
    pub(crate) fn clear_push_rev(&self, id: &str, rev: &str) -> Result<bool, DeviceError> {
        let mut cleared = false;
        self.change(|file| {
            if let Some(record) = file.devices.get_mut(id) {
                if record.push.as_ref().is_some_and(|push| push.rev == rev) {
                    record.push = None;
                    cleared = true;
                }
            }
        })?;
        Ok(cleared)
    }

    /// Remove every subscription made against a key other than `key_id` (the
    /// key was rotated). The caller holds the notification-key lock and read
    /// `key_id` under it. Returns how many were removed.
    pub(crate) fn clear_push_except_key(&self, key_id: &str) -> Result<usize, DeviceError> {
        let mut cleared = 0;
        self.change(|file| {
            for record in file.devices.values_mut() {
                if record
                    .push
                    .as_ref()
                    .is_some_and(|push| push.key_id != key_id)
                {
                    record.push = None;
                    cleared += 1;
                }
            }
        })?;
        Ok(cleared)
    }

    /// A device's current subscription, read fresh (`None` when off,
    /// unknown or revoked).
    pub(crate) fn push_of(&self, id: &str) -> Option<PushSubscription> {
        let file = self.current().ok()?;
        let record = file.devices.get(id)?;
        if record.revoked_unix.is_some() {
            return None;
        }
        record.push.clone()
    }

    /// Revoke a device everywhere. Persisted before it takes effect. Returns
    /// whether the device existed and was not already revoked.
    pub(crate) fn revoke(&self, id: &str) -> Result<bool, DeviceError> {
        Ok(matches!(self.revoke_device(id)?, RevokeOutcome::Revoked(_)))
    }

    /// Append one audit entry: when, which workspace, what happened, and to
    /// which device. Metadata only — never a credential, code or session text.
    /// Best effort: a failed write is reported on stderr and does not stop the
    /// action it records.
    pub(crate) fn audit(&self, event: &str, device: Option<&str>, detail: &str) {
        if self.disabled.is_some() {
            return;
        }
        let line = serde_json::json!({
            "at": now_unix(),
            "workspace": self.workspace,
            "event": event,
            "device": device,
            "detail": detail,
        })
        .to_string();
        let Some(dir) = &self.dir else {
            self.audit_memory
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(line);
            return;
        };
        let written = prepare_dir(dir).and_then(|()| {
            let path = dir.join("audit.jsonl");
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            restrict(&path)?;
            // One write per line, so lines from several servers never interleave.
            file.write_all(format!("{line}\n").as_bytes())
        });
        if let Err(error) = written {
            eprintln!("localpilot: could not write the device audit log: {error}");
        }
    }

    /// Whether a device record exists and is not revoked (tests).
    #[cfg(test)]
    pub(crate) fn verify_id_for_test(&self, id: &str) -> bool {
        self.current()
            .ok()
            .and_then(|file| {
                file.devices
                    .get(id)
                    .map(|record| record.revoked_unix.is_none())
            })
            .unwrap_or(false)
    }

    /// The newest audit entry for `event`: when (Unix seconds) and its
    /// detail. Reads only the log's last 64 KiB.
    pub(crate) fn last_audit(&self, event: &str) -> Option<(u64, String)> {
        use std::io::{Read as _, Seek as _};
        let text = match &self.dir {
            Some(dir) => {
                let mut file = std::fs::File::open(dir.join("audit.jsonl")).ok()?;
                let len = file.metadata().ok()?.len();
                let start = len.saturating_sub(64 * 1024);
                file.seek(std::io::SeekFrom::Start(start)).ok()?;
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).ok()?;
                String::from_utf8_lossy(&bytes).into_owned()
            }
            None => self
                .audit_memory
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .join("\n"),
        };
        text.lines().rev().find_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            (value["event"] == event).then(|| {
                (
                    value["at"].as_u64().unwrap_or(0),
                    value["detail"].as_str().unwrap_or_default().to_string(),
                )
            })
        })
    }

    /// The audit lines of an in-memory registry (tests).
    #[cfg(test)]
    pub(crate) fn audit_lines(&self) -> Vec<String> {
        self.audit_memory
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Under the registry's write lock: read the latest registry, apply `edit`,
    /// and replace the file atomically. Only a successful write changes it.
    fn change(&self, edit: impl FnOnce(&mut RegistryFile)) -> Result<(), DeviceError> {
        self.change_within(None, edit)
    }

    /// [`DeviceRegistry::change`], admitted to the write lock only before
    /// `deadline` when one is given: the lock is tried, not waited for, so a
    /// busy registry answers [`DeviceError::Busy`] with nothing changed. Once
    /// admitted the change runs to completion.
    fn change_within(
        &self,
        deadline: Option<std::time::Instant>,
        edit: impl FnOnce(&mut RegistryFile),
    ) -> Result<(), DeviceError> {
        let expired = || deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline);
        if expired() {
            return Err(DeviceError::Busy);
        }
        if let Some(reason) = &self.disabled {
            return Err(DeviceError::Unavailable(reason.clone()));
        }
        #[cfg(test)]
        if self.failing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(DeviceError::Unavailable(
                "the registry cannot be written (test)".to_string(),
            ));
        }
        let Some(dir) = &self.dir else {
            let mut memory = self.memory.lock().unwrap_or_else(PoisonError::into_inner);
            if expired() {
                return Err(DeviceError::Busy);
            }
            self.admitted_for_test(deadline);
            let mut next = memory.clone();
            edit(&mut next);
            *memory = next;
            return Ok(());
        };
        prepare_dir(dir).map_err(|error| DeviceError::Persist(error.to_string()))?;
        let lock_path = dir.join("registry.lock");
        let persist_error = |error: std::io::Error| DeviceError::Persist(error.to_string());
        loop {
            // The deadline holds for every attempt, however long preparing,
            // opening or sleeping took.
            if expired() {
                return Err(DeviceError::Busy);
            }
            self.attempting_for_test(deadline);
            let lock_file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&lock_path)
                .map_err(persist_error)?;
            restrict(&lock_path).map_err(persist_error)?;
            let mut lock = fd_lock::RwLock::new(lock_file);
            let held = match deadline {
                None => lock.write().map_err(persist_error)?,
                Some(deadline) => match lock.try_write() {
                    Ok(held) => held,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        let left = deadline.saturating_duration_since(std::time::Instant::now());
                        if left.is_zero() {
                            return Err(DeviceError::Busy);
                        }
                        std::thread::sleep(left.min(std::time::Duration::from_millis(50)));
                        continue;
                    }
                    Err(error) => return Err(persist_error(error)),
                },
            };
            // Admitted only within the deadline: a lock freed (or taken) after
            // it is released again untouched.
            if expired() {
                drop(held);
                return Err(DeviceError::Busy);
            }
            self.admitted_for_test(deadline);
            // Under the lock no other writer is replacing the file: missing
            // means never written.
            let mut next = read_registry_from(dir, true).map_err(DeviceError::Unavailable)?;
            edit(&mut next);
            let written = persist(dir, &next).map_err(persist_error);
            drop(held);
            if written.is_ok() {
                self.committed_for_test(deadline);
            }
            return written;
        }
    }

    /// Tests: the hook run once a bounded change is admitted.
    fn admitted_for_test(&self, deadline: Option<std::time::Instant>) {
        self.hook_for_test(deadline, Hook::Admitted);
    }

    /// Tests: the hook run before each attempt of a bounded change.
    fn attempting_for_test(&self, deadline: Option<std::time::Instant>) {
        self.hook_for_test(deadline, Hook::Attempting);
    }

    /// Tests: the hook run once a bounded change is written.
    fn committed_for_test(&self, deadline: Option<std::time::Instant>) {
        self.hook_for_test(deadline, Hook::Committed);
    }

    #[allow(clippy::unused_self)]
    fn hook_for_test(&self, deadline: Option<std::time::Instant>, which: Hook) {
        #[cfg(test)]
        if deadline.is_some() {
            let hook = self
                .hooks
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&which)
                .cloned();
            if let Some(hook) = hook {
                hook();
            }
        }
        #[cfg(not(test))]
        let _ = (deadline, which);
    }
}

/// Create the registry directory, restricted to the owner, with a
/// `.gitignore` that keeps it out of version control.
pub(crate) fn prepare_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    restrict(dir)?;
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(&ignore, "*\n")?;
    }
    Ok(())
}

/// Write the registry: a restricted temporary file, then an atomic rename.
/// The caller holds the write lock and has prepared the directory.
fn persist(dir: &Path, file: &RegistryFile) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(file).map_err(std::io::Error::other)?;
    replace_restricted(dir, "registry.json", &bytes)
}

/// Replace `dir/name` with `bytes`: a restricted temporary file, then an
/// atomic rename, so a reader sees the old or the new file, never a part.
/// The caller holds the file's write lock and has prepared the directory.
pub(crate) fn replace_restricted(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    let temp = dir.join(format!("{name}.{}.tmp", std::process::id()));
    {
        let mut out = std::fs::File::create(&temp)?;
        restrict(&temp)?;
        out.write_all(bytes)?;
        out.sync_all()?;
    }
    // A reader holding the file open makes Windows refuse the replacement for
    // a moment; retry rather than fail the change.
    let target = dir.join(name);
    let mut renamed = std::fs::rename(&temp, &target);
    for _ in 0..200 {
        match &renamed {
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                std::thread::sleep(std::time::Duration::from_millis(5));
                renamed = std::fs::rename(&temp, &target);
            }
            _ => break,
        }
    }
    if renamed.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    renamed
}

/// Restrict `path` to the current user.
pub(crate) fn restrict(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        localpilot_winsec::restrict_path_to_owner(path)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = if path.is_dir() { 0o700 } else { 0o600 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = path;
        Ok(())
    }
}

/// The gateway's live capabilities. A capability is valid while the control
/// connection that registered it stays open; closing it cancels the token,
/// which ends every device connection bound to it.
#[derive(Default)]
pub(crate) struct GatewayLeases {
    /// SHA-256 of each live capability, with its lease token.
    live: Mutex<HashMap<String, CancellationToken>>,
}

/// One registered gateway: its capability and the lease that ends with its
/// control connection. Dropping it revokes the capability.
pub(crate) struct GatewayLease<'a> {
    leases: &'a GatewayLeases,
    key: String,
    pub(crate) capability: Secret,
    pub(crate) token: CancellationToken,
}

impl Drop for GatewayLease<'_> {
    fn drop(&mut self) {
        // Under the leases lock, so an action running in
        // [`GatewayLeases::while_live`] either finishes first or never starts.
        let mut live = self
            .leases
            .live
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.token.cancel();
        live.remove(&self.key);
    }
}

impl GatewayLeases {
    /// Issue a capability for a new gateway control connection.
    pub(crate) fn register(&self) -> Result<GatewayLease<'_>, DeviceError> {
        let capability = format!("lpg1.{}", random_token()?);
        let key = sha256_hex(&capability);
        let token = CancellationToken::new();
        self.live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.clone(), token.clone());
        Ok(GatewayLease {
            leases: self,
            key,
            capability: Secret(capability),
            token,
        })
    }

    /// The lease of a presented capability, if it is live.
    pub(crate) fn lease(&self, capability: &str) -> Option<CancellationToken> {
        let key = sha256_hex(capability);
        let live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        Self::find(&live, &key)
    }

    /// Run `action` only if `capability` is live, holding the leases lock so
    /// the gateway's control channel cannot close part-way through: the action
    /// (registering a device, binding a connection) happens entirely under a
    /// live capability or not at all. Returns `None` when it is not live.
    pub(crate) fn while_live<T>(
        &self,
        capability: &str,
        action: impl FnOnce(&CancellationToken) -> T,
    ) -> Option<T> {
        let key = sha256_hex(capability);
        let live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
        let token = Self::find(&live, &key)?;
        Some(action(&token))
    }

    fn find(live: &HashMap<String, CancellationToken>, key: &str) -> Option<CancellationToken> {
        live.iter()
            .find(|(stored, _)| constant_time_eq(stored.as_bytes(), key.as_bytes()))
            .map(|(_, token)| token.clone())
            .filter(|token| !token.is_cancelled())
    }
}

/// How long a pairing code stays valid.
pub(crate) const PAIRING_TTL: std::time::Duration = std::time::Duration::from_secs(300);
/// Wrong secrets a pairing code survives before it is withdrawn.
const PAIRING_FAILURES: u8 = 5;
/// Pairing codes outstanding at once.
const PAIRINGS_OUTSTANDING: usize = 8;

struct PendingPairing {
    secret_sha256: String,
    name: String,
    scope: Scope,
    this_workspace_only: bool,
    expires: std::time::Instant,
    failures: u8,
    /// Set when an issuer owns this code (`pair_open`): it is told how the
    /// code ends, exactly once. `None` for a code from `pair_begin`.
    owner: Option<Owner>,
}

/// The issuer of an owned code and how to tell it the code's outcome.
struct Owner {
    stream: String,
    n: u64,
    receipt_sha256: String,
    grant_sha256: String,
    outcome: Option<tokio::sync::oneshot::Sender<localpilot_rpc::PairingOutcome>>,
}

/// Issuer streams whose replay fences a server keeps; a new stream past this
/// is refused, never an old fence forgotten.
const PAIRING_STREAMS: usize = 64;
/// Ended owned attempts whose outcome stays readable by receipt.
const FINISHED_KEPT: usize = 32;
/// How long after its code's deadline an ended attempt stays readable.
const FINISHED_FOR: std::time::Duration = std::time::Duration::from_secs(600);
/// Attempts cancelled before they were opened, remembered for status only
/// (the stream's fence is what keeps them from ever being opened).
const CANCEL_MARKERS: usize = 8;
/// Cancelled codes a phone is told were closed on the PC.
const TOMBSTONES: usize = 8;

/// An ended owned attempt: hashes and its outcome, never a secret.
struct Finished {
    stream: String,
    n: u64,
    receipt_sha256: String,
    grant_sha256: String,
    outcome: localpilot_rpc::PairingOutcome,
    kept_until: std::time::Instant,
}

/// An attempt cancelled before it was opened.
struct Marker {
    stream: String,
    n: u64,
    receipt_sha256: String,
    until: std::time::Instant,
}

/// A cancelled code: a phone presenting exactly it is told it was closed.
struct Tombstone {
    id: String,
    secret_sha256: String,
    until: std::time::Instant,
}

/// Everything pairing keeps, behind one lock so that use, cancel, expiry,
/// eviction, withdrawal and rollback are decided one at a time.
#[derive(Default)]
struct PairingBook {
    pending: HashMap<String, PendingPairing>,
    /// Per issuer stream, the highest request number taken (opened or
    /// cancelled before opening) by this server instance.
    streams: HashMap<String, u64>,
    finished: std::collections::VecDeque<Finished>,
    markers: std::collections::VecDeque<Marker>,
    tombstones: std::collections::VecDeque<Tombstone>,
}

impl PairingBook {
    /// End the pending entry `id` with `outcome`: its owner is told, and the
    /// outcome stays readable by receipt for a while. Entries without an owner
    /// simply go.
    fn finish(&mut self, id: &str, outcome: localpilot_rpc::PairingOutcome) {
        let Some(entry) = self.pending.remove(id) else {
            return;
        };
        let Some(mut owner) = entry.owner else {
            return;
        };
        if let Some(tell) = owner.outcome.take() {
            let _ = tell.send(outcome.clone());
        }
        if matches!(outcome, localpilot_rpc::PairingOutcome::Cancelled) {
            if self.tombstones.len() >= TOMBSTONES {
                self.tombstones.pop_front();
            }
            self.tombstones.push_back(Tombstone {
                id: id.to_string(),
                secret_sha256: entry.secret_sha256,
                until: entry.expires,
            });
        }
        if self.finished.len() >= FINISHED_KEPT {
            self.finished.pop_front();
        }
        self.finished.push_back(Finished {
            stream: owner.stream,
            n: owner.n,
            receipt_sha256: owner.receipt_sha256,
            grant_sha256: owner.grant_sha256,
            outcome,
            kept_until: entry.expires + FINISHED_FOR,
        });
    }

    /// Expire what is due: codes past their deadline (an owned one ends as
    /// expired) and remembered state past its time.
    fn prune(&mut self, now: std::time::Instant) {
        let due: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, pairing)| pairing.expires <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in due {
            self.finish(&id, localpilot_rpc::PairingOutcome::Expired);
        }
        self.finished.retain(|finished| finished.kept_until > now);
        self.markers.retain(|marker| marker.until > now);
        self.tombstones.retain(|tombstone| tombstone.until > now);
    }

    /// Make room for one more code by ending the one closest to expiry.
    fn make_room(&mut self) {
        if self.pending.len() < PAIRINGS_OUTSTANDING {
            return;
        }
        if let Some(oldest) = self
            .pending
            .iter()
            .min_by_key(|(_, pairing)| pairing.expires)
            .map(|(id, _)| id.clone())
        {
            self.finish(&oldest, localpilot_rpc::PairingOutcome::Evicted);
        }
    }

    /// Whether `receipt_sha256` already belongs to an attempt other than
    /// (`stream`, `n`): live, ended or cancelled before opening. A receipt
    /// names exactly one attempt.
    fn receipt_elsewhere(&self, receipt_sha256: &str, stream: &str, n: u64) -> bool {
        let same = |hash: &str| constant_time_eq(hash.as_bytes(), receipt_sha256.as_bytes());
        let other = |s: &str, m: u64| s != stream || m != n;
        self.pending
            .values()
            .filter_map(|p| p.owner.as_ref())
            .any(|o| same(&o.receipt_sha256) && other(&o.stream, o.n))
            || self
                .finished
                .iter()
                .any(|f| same(&f.receipt_sha256) && other(&f.stream, f.n))
            || self
                .markers
                .iter()
                .any(|m| same(&m.receipt_sha256) && other(&m.stream, m.n))
    }

    /// The live owned entry holding `receipt_sha256`, if any.
    fn live_by_receipt(&self, receipt_sha256: &str) -> Option<&String> {
        self.pending.iter().find_map(|(id, pairing)| {
            pairing
                .owner
                .as_ref()
                .filter(|owner| {
                    constant_time_eq(owner.receipt_sha256.as_bytes(), receipt_sha256.as_bytes())
                })
                .map(|_| id)
        })
    }
}

/// Outstanding pairing codes. A code is `<id>.<secret>`: the id (not secret)
/// names it, so a wrong secret counts against that code, which is withdrawn
/// after a few failures; the secret is 32 random bytes and only its hash is
/// kept. A code is single use, expires on a monotonic clock, and is consumed
/// together with the new device's registration.
///
/// A code opened by an issuer (`pair_open`) is owned: the issuer is told how
/// it ended, exactly once, whatever ended it, and can ask by its receipt
/// later. Its request key (stream and number, for this server instance) is
/// never opened twice: a stream's highest number is its replay fence.
pub(crate) struct Pairings {
    book: Mutex<PairingBook>,
    ttl: std::time::Duration,
}

impl Default for Pairings {
    fn default() -> Self {
        Self {
            book: Mutex::new(PairingBook::default()),
            ttl: PAIRING_TTL,
        }
    }
}

/// Why a pairing exchange failed. Deliberately coarse on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PairingRefusal {
    /// Unknown, expired, withdrawn or wrong code.
    Invalid,
    /// This exact code was closed on the PC.
    Closed,
    /// The registry could not take the new device.
    Registry(String),
}

/// What an issuer asks for when it opens a pairing.
pub(crate) struct PairOpen<'a> {
    pub(crate) instance: &'a str,
    pub(crate) stream: &'a str,
    pub(crate) n: u64,
    pub(crate) receipt: &'a str,
    pub(crate) name: &'a str,
    pub(crate) scope: Scope,
    pub(crate) this_workspace_only: bool,
}

/// What opening a pairing did.
pub(crate) enum Opened {
    /// A new code, owned by the caller; `id` names it for cancel and expiry.
    Code {
        id: String,
        code: Secret,
        lifetime: std::time::Duration,
        outcome: tokio::sync::oneshot::Receiver<localpilot_rpc::PairingOutcome>,
    },
    /// The key was already taken: what is known about it (nothing was minted).
    Known(localpilot_rpc::PairingState),
    /// Refused, with the reason.
    Refused(String),
}

impl Pairings {
    /// Pairings whose codes live for `ttl` (tests).
    #[cfg(test)]
    pub(crate) fn with_ttl(ttl: std::time::Duration) -> Self {
        Self {
            ttl,
            ..Self::default()
        }
    }

    fn book(&self) -> std::sync::MutexGuard<'_, PairingBook> {
        self.book.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Start pairing a device: a single-use code, valid for [`PAIRING_TTL`].
    pub(crate) fn begin(
        &self,
        name: &str,
        scope: Scope,
        this_workspace_only: bool,
    ) -> Result<Secret, DeviceError> {
        let id = random_hex(6)?;
        let secret = random_token()?;
        let mut book = self.book();
        let now = std::time::Instant::now();
        book.prune(now);
        book.make_room();
        book.pending.insert(
            id.clone(),
            PendingPairing {
                secret_sha256: sha256_hex(&secret),
                name: name.trim().chars().take(64).collect(),
                scope,
                this_workspace_only,
                expires: now + self.ttl,
                failures: 0,
                owner: None,
            },
        );
        Ok(Secret(format!("{id}.{secret}")))
    }

    /// Open a pairing owned by the caller, under its request key. A key at or
    /// below its stream's fence mints nothing: it answers what is known about
    /// that attempt, if the receipt and grant match, or `unknown`.
    pub(crate) fn open(&self, server_instance: &str, request: &PairOpen<'_>) -> Opened {
        use localpilot_rpc::PairingState;
        if request.instance != server_instance {
            return Opened::Refused(
                "this pairing was prepared for another server instance (the server restarted)"
                    .to_string(),
            );
        }
        let receipt_sha256 = sha256_hex(request.receipt);
        let name: String = request.name.trim().chars().take(64).collect();
        let grant_sha256 = sha256_hex(&format!(
            "{name}\u{0}{:?}\u{0}{}",
            request.scope, request.this_workspace_only
        ));
        let same = |receipt: &str, grant: &str| {
            constant_time_eq(receipt.as_bytes(), receipt_sha256.as_bytes()) && grant == grant_sha256
        };
        let mut book = self.book();
        let now = std::time::Instant::now();
        book.prune(now);
        // Already opened under this key, or cancelled before it was.
        if let Some(owner) = book
            .pending
            .values()
            .filter_map(|p| p.owner.as_ref())
            .find(|o| o.stream == request.stream && o.n == request.n)
        {
            return if same(&owner.receipt_sha256, &owner.grant_sha256) {
                Opened::Known(PairingState::Pending)
            } else {
                Opened::Refused("this request key belongs to another attempt".to_string())
            };
        }
        if let Some(finished) = book
            .finished
            .iter()
            .find(|f| f.stream == request.stream && f.n == request.n)
        {
            return if same(&finished.receipt_sha256, &finished.grant_sha256) {
                Opened::Known(PairingState::Finished {
                    outcome: finished.outcome.clone(),
                })
            } else {
                Opened::Refused("this request key belongs to another attempt".to_string())
            };
        }
        if let Some(marker) = book
            .markers
            .iter()
            .find(|m| m.stream == request.stream && m.n == request.n)
        {
            return if constant_time_eq(marker.receipt_sha256.as_bytes(), receipt_sha256.as_bytes())
            {
                Opened::Known(PairingState::CancelledBeforeOpen)
            } else {
                Opened::Refused("this request key belongs to another attempt".to_string())
            };
        }
        // A receipt names one attempt only; refused before any fence moves.
        if book.receipt_elsewhere(&receipt_sha256, request.stream, request.n) {
            return Opened::Refused("this receipt belongs to another attempt".to_string());
        }
        match book.streams.get(request.stream) {
            // Taken before (and forgotten since): never opened again.
            Some(fence) if request.n <= *fence => return Opened::Known(PairingState::Unknown),
            Some(_) => {}
            None if book.streams.len() >= PAIRING_STREAMS => {
                return Opened::Refused("too many pairing streams; restart the server".to_string());
            }
            None => {}
        }
        let (Ok(id), Ok(secret)) = (random_hex(6), random_token()) else {
            return Opened::Refused("could not create a pairing code".to_string());
        };
        book.streams.insert(request.stream.to_string(), request.n);
        book.make_room();
        let (tell, outcome) = tokio::sync::oneshot::channel();
        book.pending.insert(
            id.clone(),
            PendingPairing {
                secret_sha256: sha256_hex(&secret),
                name,
                scope: request.scope,
                this_workspace_only: request.this_workspace_only,
                expires: now + self.ttl,
                failures: 0,
                owner: Some(Owner {
                    stream: request.stream.to_string(),
                    n: request.n,
                    receipt_sha256,
                    grant_sha256,
                    outcome: Some(tell),
                }),
            },
        );
        Opened::Code {
            code: Secret(format!("{id}.{secret}")),
            id,
            lifetime: self.ttl,
            outcome,
        }
    }

    /// Cancel the owned code `id` (its issuer's connection asks).
    pub(crate) fn cancel(&self, id: &str) {
        let mut book = self.book();
        book.prune(std::time::Instant::now());
        if book.pending.get(id).is_some_and(|p| p.owner.is_some()) {
            book.finish(id, localpilot_rpc::PairingOutcome::Cancelled);
        }
    }

    /// End whatever is due (an owned code's own deadline reached).
    pub(crate) fn expire_due(&self) {
        self.book().prune(std::time::Instant::now());
    }

    /// The owned code `id`'s issuer is gone: a code still live ends.
    pub(crate) fn issuer_gone(&self, id: &str) {
        let mut book = self.book();
        if book.pending.get(id).is_some_and(|p| p.owner.is_some()) {
            book.finish(id, localpilot_rpc::PairingOutcome::IssuerGone);
        }
    }

    /// What is known about the attempt holding `receipt`.
    pub(crate) fn status(&self, receipt: &str) -> localpilot_rpc::PairingState {
        use localpilot_rpc::PairingState;
        let receipt_sha256 = sha256_hex(receipt);
        let mut book = self.book();
        book.prune(std::time::Instant::now());
        if book.live_by_receipt(&receipt_sha256).is_some() {
            return PairingState::Pending;
        }
        if let Some(finished) = book
            .finished
            .iter()
            .find(|f| constant_time_eq(f.receipt_sha256.as_bytes(), receipt_sha256.as_bytes()))
        {
            return PairingState::Finished {
                outcome: finished.outcome.clone(),
            };
        }
        if book
            .markers
            .iter()
            .any(|m| constant_time_eq(m.receipt_sha256.as_bytes(), receipt_sha256.as_bytes()))
        {
            return PairingState::CancelledBeforeOpen;
        }
        PairingState::Unknown
    }

    /// Cancel the attempt with this key and receipt: a live one ends as
    /// cancelled, an ended one answers how it ended, and one not opened yet is
    /// fenced off so it never will be.
    pub(crate) fn cancel_by_receipt(
        &self,
        server_instance: &str,
        instance: &str,
        stream: &str,
        n: u64,
        receipt: &str,
    ) -> Result<localpilot_rpc::PairingState, String> {
        use localpilot_rpc::PairingState;
        if instance != server_instance {
            return Err(
                "this pairing was prepared for another server instance (the server restarted)"
                    .to_string(),
            );
        }
        let receipt_sha256 = sha256_hex(receipt);
        let matches = |hash: &str| constant_time_eq(hash.as_bytes(), receipt_sha256.as_bytes());
        let mut book = self.book();
        let now = std::time::Instant::now();
        book.prune(now);
        let live = book.pending.iter().find_map(|(id, p)| {
            p.owner
                .as_ref()
                .filter(|o| o.stream == stream && o.n == n)
                .map(|o| (id.clone(), matches(&o.receipt_sha256)))
        });
        if let Some((id, authorized)) = live {
            if !authorized {
                return Err("this receipt does not belong to that attempt".to_string());
            }
            book.finish(&id, localpilot_rpc::PairingOutcome::Cancelled);
            return Ok(PairingState::Finished {
                outcome: localpilot_rpc::PairingOutcome::Cancelled,
            });
        }
        if let Some(finished) = book
            .finished
            .iter()
            .find(|f| f.stream == stream && f.n == n)
        {
            if !matches(&finished.receipt_sha256) {
                return Err("this receipt does not belong to that attempt".to_string());
            }
            return Ok(PairingState::Finished {
                outcome: finished.outcome.clone(),
            });
        }
        if let Some(marker) = book.markers.iter().find(|m| m.stream == stream && m.n == n) {
            if !matches(&marker.receipt_sha256) {
                return Err("this receipt does not belong to that attempt".to_string());
            }
            return Ok(PairingState::CancelledBeforeOpen);
        }
        match book.streams.get(stream) {
            Some(fence) if n <= *fence => Ok(PairingState::Unknown),
            None if book.streams.len() >= PAIRING_STREAMS => {
                Err("too many pairing streams; restart the server".to_string())
            }
            _ if book.receipt_elsewhere(&receipt_sha256, stream, n) => {
                Err("this receipt belongs to another attempt".to_string())
            }
            _ => {
                // Not opened yet: take its number so it never can be.
                book.streams.insert(stream.to_string(), n);
                if book.markers.len() >= CANCEL_MARKERS {
                    book.markers.pop_front();
                }
                book.markers.push_back(Marker {
                    stream: stream.to_string(),
                    n,
                    receipt_sha256,
                    until: now + self.ttl,
                });
                Ok(PairingState::CancelledBeforeOpen)
            }
        }
    }

    /// Exchange a pairing code for a device credential. The code is checked
    /// and consumed, and the device registered, under the pairing lock, so two
    /// exchanges of one code cannot both succeed.
    pub(crate) fn exchange(
        &self,
        code: &str,
        registry: &DeviceRegistry,
    ) -> Result<(Standing, Secret), PairingRefusal> {
        let (id, secret) = code.split_once('.').ok_or(PairingRefusal::Invalid)?;
        let mut book = self.book();
        book.prune(std::time::Instant::now());
        let Some(pairing) = book.pending.get_mut(id) else {
            // A code closed on the PC, presented exactly: say so.
            let closed = book.tombstones.iter().any(|tombstone| {
                tombstone.id == id
                    && constant_time_eq(
                        sha256_hex(secret).as_bytes(),
                        tombstone.secret_sha256.as_bytes(),
                    )
            });
            return Err(if closed {
                PairingRefusal::Closed
            } else {
                PairingRefusal::Invalid
            });
        };
        if !constant_time_eq(
            sha256_hex(secret).as_bytes(),
            pairing.secret_sha256.as_bytes(),
        ) {
            pairing.failures += 1;
            if pairing.failures >= PAIRING_FAILURES {
                book.finish(id, localpilot_rpc::PairingOutcome::Withdrawn);
            }
            return Err(PairingRefusal::Invalid);
        }
        let (name, scope, this_workspace_only) = (
            pairing.name.clone(),
            pairing.scope,
            pairing.this_workspace_only,
        );
        match registry.add(&name, scope, this_workspace_only) {
            Ok((standing, credential)) => {
                book.finish(
                    id,
                    localpilot_rpc::PairingOutcome::Used {
                        device_id: standing.id.clone(),
                        name,
                        scope: standing.scope,
                    },
                );
                Ok((standing, credential))
            }
            // Nothing was registered: the code (and its owner) stay as they
            // were.
            Err(error) => Err(PairingRefusal::Registry(error.to_string())),
        }
    }
}

/// An owned pairing code for as long as its issuer's connection is served:
/// however that ends, a code still live ends as `issuer_gone`.
pub(crate) struct OwnedPairing {
    pairings: std::sync::Arc<Pairings>,
    id: String,
}

impl OwnedPairing {
    pub(crate) fn new(pairings: std::sync::Arc<Pairings>, id: String) -> Self {
        Self { pairings, id }
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for OwnedPairing {
    fn drop(&mut self) {
        self.pairings.issuer_gone(&self.id);
    }
}

/// Failed credential attempts per presented device id, answered with a
/// growing delay. A valid credential is never refused or delayed by it.
#[derive(Default)]
pub(crate) struct FailureDelays {
    failures: Mutex<HashMap<String, u32>>,
}

/// Device ids with failure counts kept at once.
const DELAY_ENTRIES: usize = 1024;

impl FailureDelays {
    /// Record a failed attempt for `key` and return how long to wait before
    /// answering it: 100 ms doubling per failure, at most 5 s.
    pub(crate) fn failed(&self, key: &str) -> std::time::Duration {
        let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
        if !failures.contains_key(key) && failures.len() >= DELAY_ENTRIES {
            failures.clear(); // bounded memory; the delays restart
        }
        let count = failures.entry(key.to_string()).or_insert(0);
        *count = count.saturating_add(1);
        let millis = 100u64.saturating_mul(1u64 << (*count - 1).min(6));
        std::time::Duration::from_millis(millis.min(5_000))
    }

    /// Forget a key's failures after a success.
    pub(crate) fn succeeded(&self, key: &str) {
        self.failures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key);
    }
}

/// The device id a presented credential names, for keying failure delays.
pub(crate) fn presented_device_id(credential: &str) -> Option<String> {
    credential_device_id(credential).map(str::to_string)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_pairing_code_is_single_use_and_registers_one_device() {
        let registry = DeviceRegistry::default();
        let pairings = Pairings::default();
        let code = pairings.begin("phone", Scope::Control, false).unwrap();
        let (standing, credential) = pairings.exchange(code.expose(), &registry).unwrap();
        assert_eq!(standing.scope, Scope::Control);
        assert_eq!(registry.verify(credential.expose()).unwrap(), standing);
        assert_eq!(
            pairings.exchange(code.expose(), &registry),
            Err(PairingRefusal::Invalid),
            "a used code is gone"
        );
        assert_eq!(registry.list().unwrap().len(), 1);
    }

    #[test]
    fn wrong_secrets_withdraw_the_named_code() {
        let registry = DeviceRegistry::default();
        let pairings = Pairings::default();
        let code = pairings.begin("phone", Scope::Observe, false).unwrap();
        let id = code.expose().split_once('.').unwrap().0.to_string();
        for _ in 0..PAIRING_FAILURES {
            assert_eq!(
                pairings.exchange(&format!("{id}.{}", "0".repeat(64)), &registry),
                Err(PairingRefusal::Invalid)
            );
        }
        assert_eq!(
            pairings.exchange(code.expose(), &registry),
            Err(PairingRefusal::Invalid),
            "withdrawn after too many wrong secrets"
        );
        assert!(registry.list().unwrap().is_empty());
    }

    #[test]
    fn two_exchanges_of_one_code_register_one_device() {
        let registry = std::sync::Arc::new(DeviceRegistry::default());
        let pairings = std::sync::Arc::new(Pairings::default());
        let code = pairings
            .begin("phone", Scope::Observe, false)
            .unwrap()
            .expose()
            .to_string();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (registry, pairings, code) = (registry.clone(), pairings.clone(), code.clone());
                std::thread::spawn(move || pairings.exchange(&code, &registry).is_ok())
            })
            .collect();
        let wins = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .filter(|ok| *ok)
            .count();
        assert_eq!(wins, 1);
        assert_eq!(registry.list().unwrap().len(), 1);
    }

    #[test]
    fn failure_delays_grow_and_are_bounded() {
        let delays = FailureDelays::default();
        assert_eq!(delays.failed("a"), std::time::Duration::from_millis(100));
        assert_eq!(delays.failed("a"), std::time::Duration::from_millis(200));
        for _ in 0..20 {
            delays.failed("a");
        }
        assert_eq!(delays.failed("a"), std::time::Duration::from_secs(5));
        assert_eq!(
            delays.failed("b"),
            std::time::Duration::from_millis(100),
            "per key"
        );
        delays.succeeded("a");
        assert_eq!(delays.failed("a"), std::time::Duration::from_millis(100));
    }

    #[test]
    fn a_secret_never_prints() {
        let secret = Secret("lpd1.abc.very-secret".to_string());
        let shown = format!("{secret:?} {:?}", vec![secret.clone()]);
        assert!(!shown.contains("very-secret"));
        assert_eq!(secret.expose(), "lpd1.abc.very-secret");
    }

    #[test]
    fn a_device_verifies_with_its_credential_only() {
        let registry = DeviceRegistry::default();
        let (standing, credential) = registry.add("phone", Scope::Observe, false).unwrap();
        assert_eq!(registry.verify(credential.expose()).unwrap(), standing);

        // Same id, another secret; a malformed credential; an unknown id.
        let forged = format!("{CREDENTIAL_PREFIX}.{}.{}", standing.id, "a".repeat(64));
        assert_eq!(registry.verify(&forged), Err(Refusal::BadCredential));
        assert_eq!(
            registry.verify("not a credential"),
            Err(Refusal::BadCredential)
        );
        let unknown = format!("{CREDENTIAL_PREFIX}.0000000000000000.{}", "a".repeat(64));
        assert_eq!(registry.verify(&unknown), Err(Refusal::BadCredential));

        assert!(registry.revoke(&standing.id).unwrap());
        assert_eq!(registry.verify(credential.expose()), Err(Refusal::Revoked));
        assert_eq!(registry.standing(&standing.id), Err(Refusal::Revoked));
    }

    const ENDPOINT: &str = "https://fcm.googleapis.com/fcm/send/secret-path";

    #[test]
    fn a_late_push_cleanup_never_removes_a_newer_subscription() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("devices");
        let registry = DeviceRegistry::open(&dir, home.path());
        let (standing, _) = registry.add("phone", Scope::Observe, false).unwrap();
        let first = registry
            .set_push(&standing.id, ENDPOINT, "p", "a", "key1")
            .unwrap();
        assert_eq!(first.len(), 32, "128 random bits");
        assert_eq!(registry.push_of(&standing.id).unwrap().rev, first);
        // An older binary rewrites the registry without the field.
        let path = dir.join("registry.json");
        let mut raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        raw["devices"][&standing.id]
            .as_object_mut()
            .unwrap()
            .remove("push");
        std::fs::write(&path, serde_json::to_vec_pretty(&raw).unwrap()).unwrap();
        assert!(registry.push_of(&standing.id).is_none(), "reads as off");
        // The phone turns notifications on again: a new, unrelated revision.
        let second = registry
            .set_push(&standing.id, ENDPOINT, "p", "a", "key1")
            .unwrap();
        assert_ne!(second, first);
        // The push service's late "gone" answer for the first subscription.
        assert!(!registry.clear_push_rev(&standing.id, &first).unwrap());
        assert_eq!(registry.push_of(&standing.id).unwrap().rev, second);
        assert!(registry.clear_push_rev(&standing.id, &second).unwrap());
        assert!(registry.push_of(&standing.id).is_none());
    }

    #[test]
    fn revoke_and_rotation_cleanup_remove_subscriptions_and_nothing_else() {
        let registry = DeviceRegistry::default();
        let (old_key, _) = registry.add("old", Scope::Observe, false).unwrap();
        let (new_key, _) = registry.add("new", Scope::Control, false).unwrap();
        let (revoked, _) = registry.add("gone", Scope::Control, false).unwrap();
        registry
            .set_push(&old_key.id, ENDPOINT, "p", "a", "key1")
            .unwrap();
        registry
            .set_push(&new_key.id, ENDPOINT, "p", "a", "key2")
            .unwrap();
        registry
            .set_push(&revoked.id, ENDPOINT, "p", "a", "key2")
            .unwrap();
        assert!(registry.revoke(&revoked.id).unwrap());
        assert!(registry.push_of(&revoked.id).is_none(), "revoke clears it");
        assert!(matches!(
            registry.set_push(&revoked.id, ENDPOINT, "p", "a", "key2"),
            Err(PushSaveError::NotAllowed)
        ));
        assert!(matches!(
            registry.set_push("unknown", ENDPOINT, "p", "a", "key2"),
            Err(PushSaveError::NotAllowed)
        ));
        assert_eq!(registry.clear_push_except_key("key2").unwrap(), 1);
        assert!(registry.push_of(&old_key.id).is_none());
        assert!(
            registry.push_of(&new_key.id).is_some(),
            "the newest key's stays"
        );
        assert!(registry.clear_push(&new_key.id).unwrap());
        assert!(!registry.clear_push(&new_key.id).unwrap(), "already off");
        assert!(
            registry.verify_id_for_test(&new_key.id),
            "the device itself stays"
        );
    }

    #[test]
    fn a_subscription_shows_only_its_host_and_a_registry_without_one_still_reads() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("devices");
        let registry = DeviceRegistry::open(&dir, home.path());
        let (standing, credential) = registry.add("phone", Scope::Observe, false).unwrap();
        let before = std::fs::read(dir.join("registry.json")).unwrap();
        assert!(
            !String::from_utf8_lossy(&before).contains("push"),
            "nothing written while off"
        );
        registry
            .set_push(&standing.id, ENDPOINT, "the-p256dh", "the-auth", "key1")
            .unwrap();
        let shown = format!("{:?}", registry.push_of(&standing.id).unwrap());
        assert!(shown.contains("fcm.googleapis.com"), "{shown}");
        for secret in ["secret-path", "the-p256dh", "the-auth"] {
            assert!(!shown.contains(secret), "{shown}");
        }
        let listed = format!("{:?}", registry.list().unwrap());
        assert!(!listed.contains("secret-path"), "{listed}");
        // The device's credential still verifies after the field was added.
        assert_eq!(registry.verify(credential.expose()).unwrap(), standing);
    }

    #[test]
    fn the_registry_persists_hashes_only_and_restricts_its_files() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("devices");
        let registry = DeviceRegistry::open(&dir, home.path());
        let (standing, credential) = registry.add("phone", Scope::Control, false).unwrap();

        let text = std::fs::read_to_string(dir.join("registry.json")).unwrap();
        assert!(text.contains(&standing.id));
        assert!(!text.contains(credential.expose()), "no credential on disk");
        assert_eq!(
            std::fs::read_to_string(dir.join(".gitignore")).unwrap(),
            "*\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(&dir.join("registry.json")), 0o600);
        }
        #[cfg(windows)]
        {
            let dacl = localpilot_winsec::path_dacl(&dir.join("registry.json")).unwrap();
            assert!(dacl.starts_with("D:P"), "protected: {dacl}");
            assert_eq!(dacl.matches("(A;").count(), 1, "owner only: {dacl}");
        }

        // A fresh daemon reads it back.
        let reopened = DeviceRegistry::open(&dir, home.path());
        assert_eq!(reopened.verify(credential.expose()).unwrap(), standing);
    }

    #[test]
    fn an_unreadable_registry_disables_devices_and_is_never_replaced() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("devices");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("registry.json"), b"{ not json").unwrap();

        let registry = DeviceRegistry::open(&dir, home.path());
        assert!(registry.unavailable().is_some());
        let any = format!("{CREDENTIAL_PREFIX}.0000000000000000.{}", "a".repeat(64));
        assert_eq!(registry.verify(&any), Err(Refusal::Unavailable));
        assert!(matches!(
            registry.add("phone", Scope::Observe, false),
            Err(DeviceError::Unavailable(_))
        ));
        // The damaged file is left for the owner to inspect, not overwritten
        // with an empty trust store.
        assert_eq!(
            std::fs::read(dir.join("registry.json")).unwrap(),
            b"{ not json"
        );
    }

    #[test]
    fn a_failed_write_leaves_the_registry_unchanged() {
        let home = tempfile::tempdir().unwrap();
        // A file where the devices directory should be makes every write fail.
        std::fs::write(home.path().join("devices"), b"in the way").unwrap();
        let registry = DeviceRegistry::open(&home.path().join("devices"), home.path());
        assert!(matches!(
            registry.add("phone", Scope::Observe, false),
            Err(DeviceError::Persist(_))
        ));
        assert!(registry.list().is_err() || registry.list().unwrap().is_empty());
    }

    #[test]
    fn one_pairing_reaches_every_workspace_and_a_revocation_reaches_them_all() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("devices");
        let (one, two) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        // Two workspaces' servers share the user's registry.
        let first = DeviceRegistry::open(&dir, one.path());
        let second = DeviceRegistry::open(&dir, two.path());

        let (standing, credential) = first.add("phone", Scope::Control, false).unwrap();
        assert_eq!(second.verify(credential.expose()).unwrap(), standing);

        // Revoked from the second workspace, refused in the first at once.
        assert!(second.revoke(&standing.id).unwrap());
        assert_eq!(first.verify(credential.expose()), Err(Refusal::Revoked));
        assert_eq!(first.standing(&standing.id), Err(Refusal::Revoked));
    }

    #[test]
    fn a_device_limited_to_one_workspace_is_refused_elsewhere() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("devices");
        let (one, two) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let first = DeviceRegistry::open(&dir, one.path());
        let second = DeviceRegistry::open(&dir, two.path());
        let (standing, credential) = first.add("tablet", Scope::Observe, true).unwrap();
        assert_eq!(first.verify(credential.expose()).unwrap(), standing);
        assert_eq!(second.verify(credential.expose()), Err(Refusal::NotGranted));
    }

    #[test]
    fn concurrent_writers_from_several_servers_lose_no_device() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("devices");
        let handles: Vec<_> = (0..6)
            .map(|n| {
                let dir = dir.clone();
                let root = home.path().to_path_buf();
                std::thread::spawn(move || {
                    let registry = DeviceRegistry::open(&dir, &root);
                    for k in 0..5 {
                        registry
                            .add(&format!("device {n}-{k}"), Scope::Observe, false)
                            .unwrap();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let registry = DeviceRegistry::open(&dir, home.path());
        assert_eq!(registry.list().unwrap().len(), 30, "every write survived");
    }

    /// Environment for the writer-process test's children.
    const CHILD_DIR: &str = "LOCALPILOT_TEST_DEVICE_REGISTRY_DIR";
    const CHILD_TASK: &str = "LOCALPILOT_TEST_DEVICE_REGISTRY_TASK";

    /// Not a test on its own: the body the writer-process test runs in child
    /// processes (it does nothing without the environment above).
    #[test]
    fn registry_child_process() {
        let (Ok(dir), Ok(task)) = (std::env::var(CHILD_DIR), std::env::var(CHILD_TASK)) else {
            return;
        };
        let dir = PathBuf::from(dir);
        let registry = DeviceRegistry::open(&dir, &dir);
        if let Some(id) = task.strip_prefix("revoke:") {
            assert!(registry.revoke(id).unwrap());
        } else {
            for k in 0..5 {
                registry
                    .add(&format!("{task}-{k}"), Scope::Observe, false)
                    .unwrap();
            }
        }
    }

    fn run_child(dir: &Path, task: &str) -> std::process::Child {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "server_devices::tests::registry_child_process",
                "--quiet",
            ])
            .env(CHILD_DIR, dir)
            .env(CHILD_TASK, task)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    fn separate_processes_share_writes_and_revocations() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("devices");
        let children: Vec<_> = (0..4).map(|n| run_child(&dir, &format!("p{n}"))).collect();
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }
        let registry = DeviceRegistry::open(&dir, home.path());
        assert_eq!(
            registry.list().unwrap().len(),
            20,
            "every process's writes survived"
        );

        let (standing, credential) = registry.add("phone", Scope::Control, false).unwrap();
        let mut child = run_child(&dir, &format!("revoke:{}", standing.id));
        assert!(child.wait().unwrap().success());
        assert_eq!(registry.verify(credential.expose()), Err(Refusal::Revoked));
    }

    #[test]
    fn a_read_during_writes_never_sees_an_empty_registry() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("devices");
        let writer = DeviceRegistry::open(&dir, home.path());
        writer.add("first", Scope::Observe, false).unwrap();
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader = {
            let (dir, root, done) = (dir.clone(), home.path().to_path_buf(), done.clone());
            std::thread::spawn(move || {
                let registry = DeviceRegistry::open(&dir, &root);
                let mut last = 1;
                while !done.load(std::sync::atomic::Ordering::SeqCst) {
                    let count = registry.list().expect("readable throughout").len();
                    assert!(count >= last, "never fewer devices than before");
                    last = count;
                }
            })
        };
        for k in 0..40 {
            writer.add(&format!("d{k}"), Scope::Observe, false).unwrap();
        }
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        reader.join().unwrap();
    }

    #[test]
    fn a_workspace_grant_matches_any_spelling_of_the_root() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("devices");
        let project = home.path().join("project");
        std::fs::create_dir_all(project.join("sub")).unwrap();
        let canonical = DeviceRegistry::open(&dir, &project);
        let (standing, credential) = canonical.add("tablet", Scope::Observe, true).unwrap();
        // The same directory reached through another spelling.
        let aliased = DeviceRegistry::open(&dir, &project.join("sub").join(".."));
        assert_eq!(aliased.verify(credential.expose()).unwrap(), standing);
    }

    #[test]
    fn without_a_home_directory_devices_are_unavailable_not_relocated() {
        let root = tempfile::tempdir().unwrap();
        let registry = DeviceRegistry {
            disabled: Some("no home".to_string()),
            ..DeviceRegistry::open(&root.path().join("devices"), root.path())
        };
        assert!(registry.unavailable().is_some());
        assert!(matches!(
            registry.add("phone", Scope::Observe, false),
            Err(DeviceError::Unavailable(_))
        ));
        assert!(
            !root.path().join("devices").exists(),
            "nothing written elsewhere"
        );
    }

    #[test]
    fn nothing_runs_under_a_capability_once_its_channel_closed() {
        let leases = GatewayLeases::default();
        let lease = leases.register().unwrap();
        let capability = lease.capability.expose().to_string();
        assert_eq!(leases.while_live(&capability, |_| 1), Some(1));
        drop(lease);
        assert_eq!(leases.while_live(&capability, |_| 1), None);
    }

    #[test]
    fn a_gateway_capability_lives_exactly_as_long_as_its_lease() {
        let leases = GatewayLeases::default();
        let lease = leases.register().unwrap();
        let capability = lease.capability.expose().to_string();
        let token = leases.lease(&capability).expect("live while registered");
        assert!(leases.lease("lpg1.forged").is_none());
        drop(lease);
        assert!(token.is_cancelled(), "bound device connections end");
        assert!(
            leases.lease(&capability).is_none(),
            "and it cannot be reused"
        );
    }

    // ---- owned pairings ----------------------------------------------------

    use localpilot_rpc::{PairingOutcome, PairingState};

    const INSTANCE: &str = "this-server";

    fn request<'a>(stream: &'a str, n: u64, receipt: &'a str) -> PairOpen<'a> {
        PairOpen {
            instance: INSTANCE,
            stream,
            n,
            receipt,
            name: "phone",
            scope: Scope::Control,
            this_workspace_only: false,
        }
    }

    struct Live {
        id: String,
        code: Secret,
        outcome: tokio::sync::oneshot::Receiver<PairingOutcome>,
    }

    fn open(pairings: &Pairings, stream: &str, n: u64, receipt: &str) -> Live {
        match pairings.open(INSTANCE, &request(stream, n, receipt)) {
            Opened::Code {
                id, code, outcome, ..
            } => Live { id, code, outcome },
            Opened::Known(state) => panic!("expected a code, got {state:?}"),
            Opened::Refused(why) => panic!("expected a code, refused: {why}"),
        }
    }

    fn known(pairings: &Pairings, stream: &str, n: u64, receipt: &str) -> PairingState {
        match pairings.open(INSTANCE, &request(stream, n, receipt)) {
            Opened::Known(state) => state,
            Opened::Code { .. } => panic!("a code was minted for {stream}/{n}"),
            Opened::Refused(why) => panic!("refused: {why}"),
        }
    }

    #[test]
    fn every_way_an_owned_code_ends_is_told_once() {
        let registry = DeviceRegistry::default();
        let pairings = Pairings::default();
        // Used.
        let mut used = open(&pairings, "s", 1, "r1");
        let (standing, _) = pairings.exchange(used.code.expose(), &registry).unwrap();
        assert!(
            matches!(used.outcome.try_recv(), Ok(PairingOutcome::Used { device_id, .. }) if device_id == standing.id)
        );
        // Cancelled.
        let mut cancelled = open(&pairings, "s", 2, "r2");
        pairings.cancel(&cancelled.id);
        pairings.cancel(&cancelled.id);
        assert_eq!(cancelled.outcome.try_recv(), Ok(PairingOutcome::Cancelled));
        // Withdrawn after wrong secrets.
        let mut withdrawn = open(&pairings, "s", 3, "r3");
        let wrong = format!("{}.wrong", withdrawn.id);
        for _ in 0..PAIRING_FAILURES {
            assert_eq!(
                pairings.exchange(&wrong, &registry).unwrap_err(),
                PairingRefusal::Invalid
            );
        }
        assert_eq!(withdrawn.outcome.try_recv(), Ok(PairingOutcome::Withdrawn));
        // Issuer gone.
        let mut gone = open(&pairings, "s", 4, "r4");
        drop(OwnedPairing::new(
            std::sync::Arc::new(Pairings::default()),
            "other".into(),
        ));
        pairings.issuer_gone(&gone.id);
        assert_eq!(gone.outcome.try_recv(), Ok(PairingOutcome::IssuerGone));
        // Evicted to make room for newer codes.
        let mut evicted = open(&pairings, "s", 5, "r5");
        for _ in 0..PAIRINGS_OUTSTANDING {
            pairings.begin("cli", Scope::Observe, false).unwrap();
        }
        assert_eq!(evicted.outcome.try_recv(), Ok(PairingOutcome::Evicted));
        // Each is readable by receipt.
        assert!(matches!(
            pairings.status("r1"),
            PairingState::Finished {
                outcome: PairingOutcome::Used { .. }
            }
        ));
        assert_eq!(
            pairings.status("r2"),
            PairingState::Finished {
                outcome: PairingOutcome::Cancelled
            }
        );
        assert_eq!(
            pairings.status("r5"),
            PairingState::Finished {
                outcome: PairingOutcome::Evicted
            }
        );
    }

    #[test]
    fn an_owned_code_expires_at_its_own_deadline() {
        let pairings = Pairings::with_ttl(std::time::Duration::from_millis(20));
        let mut live = open(&pairings, "s", 1, "r");
        std::thread::sleep(std::time::Duration::from_millis(40));
        pairings.expire_due();
        assert_eq!(live.outcome.try_recv(), Ok(PairingOutcome::Expired));
        assert_eq!(
            pairings.status("r"),
            PairingState::Finished {
                outcome: PairingOutcome::Expired
            }
        );
    }

    #[test]
    fn a_registry_failure_leaves_the_code_and_its_owner_as_they_were() {
        let registry = DeviceRegistry::default();
        let pairings = Pairings::default();
        let mut live = open(&pairings, "s", 1, "r");
        registry.fail_for_test(true);
        assert!(matches!(
            pairings.exchange(live.code.expose(), &registry),
            Err(PairingRefusal::Registry(_))
        ));
        assert!(live.outcome.try_recv().is_err(), "nothing was told");
        assert_eq!(pairings.status("r"), PairingState::Pending);
        registry.fail_for_test(false);
        assert!(pairings.exchange(live.code.expose(), &registry).is_ok());
        assert!(matches!(
            live.outcome.try_recv(),
            Ok(PairingOutcome::Used { .. })
        ));
    }

    #[test]
    fn a_used_or_reopened_key_never_mints_again() {
        let registry = DeviceRegistry::default();
        let pairings = Pairings::default();
        let live = open(&pairings, "s", 1, "r1");
        // The same key while live: what is known, nothing new.
        assert_eq!(known(&pairings, "s", 1, "r1"), PairingState::Pending);
        // The same key with another receipt or grant is refused.
        assert!(matches!(
            pairings.open(INSTANCE, &request("s", 1, "other")),
            Opened::Refused(_)
        ));
        let mut changed = request("s", 1, "r1");
        changed.scope = Scope::Observe;
        assert!(matches!(
            pairings.open(INSTANCE, &changed),
            Opened::Refused(_)
        ));
        pairings.exchange(live.code.expose(), &registry).unwrap();
        assert!(matches!(
            known(&pairings, "s", 1, "r1"),
            PairingState::Finished { .. }
        ));
        // A lower number on the stream: never minted.
        let _two = open(&pairings, "s", 3, "r3");
        assert_eq!(known(&pairings, "s", 2, "r2"), PairingState::Unknown);
        // Another server instance cannot mint here.
        let mut elsewhere = request("s", 9, "r9");
        elsewhere.instance = "the-old-server";
        assert!(
            matches!(pairings.open(INSTANCE, &elsewhere), Opened::Refused(why) if why.contains("restarted"))
        );
        assert_eq!(registry.list().unwrap().len(), 1, "one device only");
    }

    #[test]
    fn a_cancel_before_open_fences_the_attempt_even_after_its_marker_is_gone() {
        let pairings = Pairings::with_ttl(std::time::Duration::from_millis(20));
        assert_eq!(
            pairings.cancel_by_receipt(INSTANCE, INSTANCE, "s", 1, "r1"),
            Ok(PairingState::CancelledBeforeOpen)
        );
        assert_eq!(pairings.status("r1"), PairingState::CancelledBeforeOpen);
        assert_eq!(
            known(&pairings, "s", 1, "r1"),
            PairingState::CancelledBeforeOpen
        );
        // More pre-open cancels than markers kept: the earliest is forgotten,
        // its fence is not.
        for n in 2..=(CANCEL_MARKERS as u64 + 3) {
            pairings
                .cancel_by_receipt(INSTANCE, INSTANCE, "s", n, &format!("r{n}"))
                .unwrap();
        }
        assert_eq!(known(&pairings, "s", 1, "r1"), PairingState::Unknown);
        // The marker's time passes: still never opened.
        pairings
            .cancel_by_receipt(INSTANCE, INSTANCE, "t", 1, "t1")
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(40));
        assert_eq!(known(&pairings, "t", 1, "t1"), PairingState::Unknown);
        assert!(
            pairings.book().pending.is_empty(),
            "nothing was ever minted"
        );
    }

    #[test]
    fn reconciling_by_receipt_cancels_live_codes_and_reports_ended_ones() {
        let registry = DeviceRegistry::default();
        let pairings = Pairings::default();
        let mut live = open(&pairings, "s", 1, "r1");
        assert_eq!(pairings.status("r1"), PairingState::Pending);
        // The wrong receipt for that attempt is refused and changes nothing.
        assert!(pairings
            .cancel_by_receipt(INSTANCE, INSTANCE, "s", 1, "nope")
            .is_err());
        assert_eq!(pairings.status("r1"), PairingState::Pending);
        assert_eq!(
            pairings.cancel_by_receipt(INSTANCE, INSTANCE, "s", 1, "r1"),
            Ok(PairingState::Finished {
                outcome: PairingOutcome::Cancelled
            })
        );
        assert_eq!(live.outcome.try_recv(), Ok(PairingOutcome::Cancelled));
        // A used one answers that it was used, and stays used.
        let used = open(&pairings, "s", 2, "r2");
        pairings.exchange(used.code.expose(), &registry).unwrap();
        assert!(matches!(
            pairings.cancel_by_receipt(INSTANCE, INSTANCE, "s", 2, "r2"),
            Ok(PairingState::Finished {
                outcome: PairingOutcome::Used { .. }
            })
        ));
        assert_eq!(pairings.status("never-seen"), PairingState::Unknown);
        assert!(pairings
            .cancel_by_receipt(INSTANCE, "the-old-server", "s", 3, "r3")
            .is_err());
    }

    #[test]
    fn ended_attempts_are_forgotten_oldest_first_but_never_reopened() {
        let pairings = Pairings::default();
        for n in 1..=(FINISHED_KEPT as u64 + 2) {
            let live = open(&pairings, "s", n, &format!("r{n}"));
            pairings.cancel(&live.id);
        }
        // Forgotten before its code would have expired: unknown, not a guess.
        assert_eq!(pairings.status("r1"), PairingState::Unknown);
        assert_eq!(known(&pairings, "s", 1, "r1"), PairingState::Unknown);
        assert_eq!(
            pairings.status(&format!("r{}", FINISHED_KEPT + 2)),
            PairingState::Finished {
                outcome: PairingOutcome::Cancelled
            }
        );
    }

    #[test]
    fn a_live_attempt_stays_readable_through_heavy_churn() {
        let pairings = Pairings::default();
        let _live = open(&pairings, "keep", 1, "keep");
        for n in 1..=40u64 {
            let other = open(&pairings, "churn", n, &format!("c{n}"));
            pairings.cancel(&other.id);
        }
        assert_eq!(pairings.status("keep"), PairingState::Pending);
    }

    #[test]
    fn new_streams_past_the_cap_are_refused_and_old_fences_stay() {
        let pairings = Pairings::default();
        for n in 0..PAIRING_STREAMS {
            pairings
                .cancel_by_receipt(INSTANCE, INSTANCE, &format!("s{n}"), 5, &format!("r{n}"))
                .unwrap();
        }
        assert!(matches!(
            pairings.open(INSTANCE, &request("one-more", 1, "x")),
            Opened::Refused(_)
        ));
        assert!(pairings
            .cancel_by_receipt(INSTANCE, INSTANCE, "one-more", 1, "x")
            .is_err());
        assert_eq!(
            known(&pairings, "s0", 3, "old"),
            PairingState::Unknown,
            "the fence holds"
        );
    }

    #[test]
    fn a_phone_is_told_a_code_was_closed_only_for_that_exact_code() {
        let registry = DeviceRegistry::default();
        let pairings = Pairings::default();
        let first = open(&pairings, "s", 1, "r1");
        pairings.cancel(&first.id);
        let id = first.id.clone();
        assert_eq!(
            pairings
                .exchange(first.code.expose(), &registry)
                .unwrap_err(),
            PairingRefusal::Closed
        );
        // The id with a wrong secret: the usual refusal.
        assert_eq!(
            pairings
                .exchange(&format!("{id}.wrong"), &registry)
                .unwrap_err(),
            PairingRefusal::Invalid
        );
        // More cancellations than kept: the earliest falls back to the usual
        // refusal, and is still never usable.
        for n in 2..=(TOMBSTONES as u64 + 1) {
            let live = open(&pairings, "s", n, &format!("r{n}"));
            pairings.cancel(&live.id);
        }
        assert_eq!(
            pairings
                .exchange(first.code.expose(), &registry)
                .unwrap_err(),
            PairingRefusal::Invalid
        );
        assert!(registry.list().unwrap().is_empty());
    }

    #[test]
    fn a_receipt_names_exactly_one_attempt() {
        let registry = DeviceRegistry::default();
        let pairings = Pairings::default();
        // One attempt with this receipt ends; another key cannot take it.
        let first = open(&pairings, "s", 1, "shared");
        pairings.exchange(first.code.expose(), &registry).unwrap();
        assert!(matches!(
            pairings.open(INSTANCE, &request("s", 2, "shared")),
            Opened::Refused(why) if why.contains("another attempt")
        ));
        // Nor while one is live.
        let _live = open(&pairings, "t", 1, "live");
        assert!(matches!(
            pairings.open(INSTANCE, &request("t", 2, "live")),
            Opened::Refused(_)
        ));
        assert!(pairings
            .cancel_by_receipt(INSTANCE, INSTANCE, "t", 3, "live")
            .is_err());
        // Nor one cancelled before it was opened.
        pairings
            .cancel_by_receipt(INSTANCE, INSTANCE, "u", 1, "early")
            .unwrap();
        assert!(matches!(
            pairings.open(INSTANCE, &request("u", 2, "early")),
            Opened::Refused(_)
        ));
        // The refusals moved no fence: the next numbers still open.
        let _next = open(&pairings, "s", 2, "fresh-s2");
        let _next = open(&pairings, "t", 2, "fresh-t2");
        // Each receipt still reads its own attempt.
        assert!(matches!(
            pairings.status("shared"),
            PairingState::Finished {
                outcome: PairingOutcome::Used { .. }
            }
        ));
        assert_eq!(pairings.status("live"), PairingState::Pending);
    }
}
