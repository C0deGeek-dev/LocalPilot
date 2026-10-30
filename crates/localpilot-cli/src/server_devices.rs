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
fn random_hex(n: usize) -> Result<String, DeviceError> {
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
fn credential_device_id(credential: &str) -> Option<&str> {
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
}

impl Default for DeviceRegistry {
    fn default() -> Self {
        Self {
            dir: None,
            memory: Mutex::new(RegistryFile::empty()),
            audit_memory: Mutex::new(Vec::new()),
            workspace: String::new(),
            disabled: None,
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

    /// The device's current standing by id, re-read before each command.
    pub(crate) fn standing(&self, id: &str) -> Result<Standing, Refusal> {
        let file = self.current().map_err(|_| Refusal::Unavailable)?;
        let record = file.devices.get(id).ok_or(Refusal::BadCredential)?;
        self.standing_of(record)
    }

    /// Every device, revoked ones included, newest first.
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
        };
        self.change(|file| {
            file.devices.insert(id.clone(), record);
        })?;
        Ok((Standing { id, scope }, Secret(credential)))
    }

    /// Revoke a device everywhere. Persisted before it takes effect. Returns
    /// whether the device existed and was not already revoked.
    pub(crate) fn revoke(&self, id: &str) -> Result<bool, DeviceError> {
        let mut changed = false;
        self.change(|file| {
            if let Some(record) = file.devices.get_mut(id) {
                if record.revoked_unix.is_none() {
                    record.revoked_unix = Some(now_unix());
                    changed = true;
                }
            }
        })?;
        Ok(changed)
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
        if let Some(reason) = &self.disabled {
            return Err(DeviceError::Unavailable(reason.clone()));
        }
        let Some(dir) = &self.dir else {
            let mut memory = self.memory.lock().unwrap_or_else(PoisonError::into_inner);
            let mut next = memory.clone();
            edit(&mut next);
            *memory = next;
            return Ok(());
        };
        prepare_dir(dir).map_err(|error| DeviceError::Persist(error.to_string()))?;
        let lock_path = dir.join("registry.lock");
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|error| DeviceError::Persist(error.to_string()))?;
        restrict(&lock_path).map_err(|error| DeviceError::Persist(error.to_string()))?;
        let mut lock = fd_lock::RwLock::new(lock_file);
        let _held = lock
            .write()
            .map_err(|error| DeviceError::Persist(error.to_string()))?;
        // Under the lock no other writer is replacing the file: missing means
        // never written.
        let mut next = read_registry_from(dir, true).map_err(DeviceError::Unavailable)?;
        edit(&mut next);
        persist(dir, &next).map_err(|error| DeviceError::Persist(error.to_string()))
    }
}

/// Create the registry directory, restricted to the owner, with a
/// `.gitignore` that keeps it out of version control.
fn prepare_dir(dir: &Path) -> std::io::Result<()> {
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
    let temp = dir.join(format!("registry.json.{}.tmp", std::process::id()));
    {
        let mut out = std::fs::File::create(&temp)?;
        restrict(&temp)?;
        out.write_all(&bytes)?;
        out.sync_all()?;
    }
    // A reader holding the file open makes Windows refuse the replacement for
    // a moment; retry rather than fail the change.
    let target = dir.join("registry.json");
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
fn restrict(path: &Path) -> std::io::Result<()> {
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
}

/// Outstanding pairing codes. A code is `<id>.<secret>`: the id (not secret)
/// names it, so a wrong secret counts against that code, which is withdrawn
/// after a few failures; the secret is 32 random bytes and only its hash is
/// kept. A code is single use, expires on a monotonic clock, and is consumed
/// together with the new device's registration.
#[derive(Default)]
pub(crate) struct Pairings {
    pending: Mutex<HashMap<String, PendingPairing>>,
}

/// Why a pairing exchange failed. Deliberately coarse on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PairingRefusal {
    /// Unknown, expired, withdrawn or wrong code.
    Invalid,
    /// The registry could not take the new device.
    Registry(String),
}

impl Pairings {
    /// Start pairing a device: a single-use code, valid for [`PAIRING_TTL`].
    pub(crate) fn begin(
        &self,
        name: &str,
        scope: Scope,
        this_workspace_only: bool,
    ) -> Result<Secret, DeviceError> {
        let id = random_hex(6)?;
        let secret = random_token()?;
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        let now = std::time::Instant::now();
        pending.retain(|_, pairing| pairing.expires > now);
        if pending.len() >= PAIRINGS_OUTSTANDING {
            // Make room by withdrawing the code closest to expiry.
            if let Some(oldest) = pending
                .iter()
                .min_by_key(|(_, pairing)| pairing.expires)
                .map(|(id, _)| id.clone())
            {
                pending.remove(&oldest);
            }
        }
        pending.insert(
            id.clone(),
            PendingPairing {
                secret_sha256: sha256_hex(&secret),
                name: name.trim().chars().take(64).collect(),
                scope,
                this_workspace_only,
                expires: now + PAIRING_TTL,
                failures: 0,
            },
        );
        Ok(Secret(format!("{id}.{secret}")))
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
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(pairing) = pending.get_mut(id) else {
            return Err(PairingRefusal::Invalid);
        };
        if pairing.expires <= std::time::Instant::now() {
            pending.remove(id);
            return Err(PairingRefusal::Invalid);
        }
        if !constant_time_eq(
            sha256_hex(secret).as_bytes(),
            pairing.secret_sha256.as_bytes(),
        ) {
            pairing.failures += 1;
            if pairing.failures >= PAIRING_FAILURES {
                pending.remove(id);
            }
            return Err(PairingRefusal::Invalid);
        }
        let Some(pairing) = pending.remove(id) else {
            return Err(PairingRefusal::Invalid);
        };
        match registry.add(&pairing.name, pairing.scope, pairing.this_workspace_only) {
            Ok(paired) => Ok(paired),
            Err(error) => {
                // Nothing was registered: the owner's code stays usable.
                pending.insert(id.to_string(), pairing);
                Err(PairingRefusal::Registry(error.to_string()))
            }
        }
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
}
