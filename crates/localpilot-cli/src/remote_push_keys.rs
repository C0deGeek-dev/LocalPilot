//! The push key: the P-256 key this computer signs Web Push requests with
//! (VAPID). It is made the first time it is needed, kept owner-only beside the
//! device registry as PKCS#8 (`vapid.p8`), and replaced by a rotation.
//!
//! Creating and rotating hold `vapid.lock`, a lock file that is never
//! replaced. Lock order, everywhere: this key lock first, then the registry
//! lock, never the other way round. The file itself is replaced atomically,
//! so a reader without the lock sees the old key or the new one.

use std::path::{Path, PathBuf};

use ring::rand::SystemRandom;
use ring::signature::{EcdsaKeyPair, KeyPair as _, ECDSA_P256_SHA256_FIXED_SIGNING};
use sha2::Digest as _;

use crate::server_devices::{prepare_dir, replace_restricted, restrict, user_devices_dir};

const KEY_FILE: &str = "vapid.p8";
const LOCK_FILE: &str = "vapid.lock";

/// Why the push key could not be read or written.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PushKeyError {
    #[error("no home directory to keep the push key in")]
    NoHome,
    #[error("the push key could not be written: {0}")]
    Write(std::io::Error),
    #[error("the push key could not be read: {0}")]
    Read(std::io::Error),
    #[error("the push key file is not a valid key")]
    Invalid,
    #[error("the system random source failed")]
    Random,
}

/// The loaded push key. Its private half never leaves this value.
pub(crate) struct PushKey {
    pair: EcdsaKeyPair,
    id: String,
}

impl std::fmt::Debug for PushKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PushKey")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl PushKey {
    fn from_pkcs8(pkcs8: &[u8]) -> Result<Self, PushKeyError> {
        let pair = EcdsaKeyPair::from_pkcs8(
            &ECDSA_P256_SHA256_FIXED_SIGNING,
            pkcs8,
            &SystemRandom::new(),
        )
        .map_err(|_| PushKeyError::Invalid)?;
        let id = key_id(pair.public_key().as_ref());
        Ok(Self { pair, id })
    }

    /// The signing key.
    pub(crate) fn pair(&self) -> &EcdsaKeyPair {
        &self.pair
    }

    /// The uncompressed public key the browser subscribes with.
    pub(crate) fn public_key(&self) -> &[u8] {
        self.pair.public_key().as_ref()
    }

    /// A short identity of this key (the first 16 hex characters of the
    /// public key's SHA-256): a subscription made against another key no
    /// longer works.
    pub(crate) fn id(&self) -> &str {
        &self.id
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            use std::fmt::Write as _;
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn key_id(public: &[u8]) -> String {
    let digest = sha2::Sha256::digest(public);
    hex(&digest[..8])
}

/// Where the push key lives.
pub(crate) struct PushKeys {
    dir: PathBuf,
}

impl PushKeys {
    /// The user's key, beside the device registry.
    pub(crate) fn user() -> Result<Self, PushKeyError> {
        user_devices_dir().map(Self::at).ok_or(PushKeyError::NoHome)
    }

    pub(crate) fn at(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The current key, or `None` if none was made yet. Takes no lock.
    pub(crate) fn current(&self) -> Result<Option<PushKey>, PushKeyError> {
        read_key(&self.dir)
    }

    /// The current key, made now if there is none.
    pub(crate) fn ensure(&self) -> Result<PushKey, PushKeyError> {
        self.locked(|dir| match read_key(dir)? {
            Some(key) => Ok(key),
            None => write_new(dir),
        })?
    }

    /// Replace the key with a new one, then remove every subscription made
    /// against another key from `registry`, all under the key lock (so a
    /// subscription made against the new key is never removed, even by a
    /// late cleanup of an earlier rotation).
    pub(crate) fn rotate(
        &self,
        registry: &crate::server_devices::DeviceRegistry,
    ) -> Result<(PushKey, usize), PushKeyError> {
        self.locked(|dir| {
            let key = write_new(dir)?;
            let removed = registry
                .clear_push_except_key(key.id())
                .map_err(|error| PushKeyError::Write(std::io::Error::other(error.to_string())))?;
            Ok((key, removed))
        })?
    }

    /// Run `work` while holding the key lock. Lock order: a caller that also
    /// changes the device registry does so inside `work`, never the reverse.
    pub(crate) fn locked<T>(&self, work: impl FnOnce(&Path) -> T) -> Result<T, PushKeyError> {
        prepare_dir(&self.dir).map_err(PushKeyError::Write)?;
        let lock_path = self.dir.join(LOCK_FILE);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(PushKeyError::Write)?;
        restrict(&lock_path).map_err(PushKeyError::Write)?;
        let mut lock = fd_lock::RwLock::new(file);
        let held = lock.write().map_err(PushKeyError::Write)?;
        let result = work(&self.dir);
        drop(held);
        Ok(result)
    }
}

fn read_key(dir: &Path) -> Result<Option<PushKey>, PushKeyError> {
    match std::fs::read(dir.join(KEY_FILE)) {
        Ok(bytes) => PushKey::from_pkcs8(&bytes).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(PushKeyError::Read(error)),
    }
}

fn write_new(dir: &Path) -> Result<PushKey, PushKeyError> {
    let pkcs8 =
        EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
            .map_err(|_| PushKeyError::Random)?;
    let key = PushKey::from_pkcs8(pkcs8.as_ref())?;
    replace_restricted(dir, KEY_FILE, pkcs8.as_ref()).map_err(PushKeyError::Write)?;
    Ok(key)
}

/// `localpilot remote push rotate`.
pub(crate) fn rotate_command() -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let registry = crate::server_devices::DeviceRegistry::for_user(&cwd);
    let (key, removed) = PushKeys::user()?.rotate(&registry)?;
    println!("New push key {}.", key.id());
    match removed {
        0 => println!("No phone had notifications on."),
        1 => println!("1 phone had notifications on; it must turn them on again."),
        n => println!("{n} phones had notifications on; they must turn them on again."),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn the_key_is_made_once_kept_and_replaced_only_by_a_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let keys = PushKeys::at(dir.path().join("devices"));
        assert!(keys.current().unwrap().is_none(), "nothing made yet");
        let first = keys.ensure().unwrap();
        assert_eq!(first.public_key().len(), 65);
        assert_eq!(first.id().len(), 16);
        assert_eq!(keys.ensure().unwrap().id(), first.id(), "ensure keeps it");
        assert_eq!(keys.current().unwrap().unwrap().id(), first.id());
        // The loaded key signs, and the signature carries its public key.
        let header =
            crate::remote_push_crypto::vapid_authorization(first.pair(), "https://x.test", 1)
                .unwrap();
        assert!(header.ends_with(&base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            first.public_key()
        )));
        let registry = crate::server_devices::DeviceRegistry::default();
        let (rotated, _) = keys.rotate(&registry).unwrap();
        assert_ne!(rotated.id(), first.id());
        assert_eq!(keys.current().unwrap().unwrap().id(), rotated.id());
    }

    #[test]
    fn the_key_never_shows_in_debug_output_and_a_bad_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let keys = PushKeys::at(dir.path().join("devices"));
        let key = keys.ensure().unwrap();
        let pkcs8 = std::fs::read(dir.path().join("devices").join(KEY_FILE)).unwrap();
        let shown = format!("{key:?}");
        assert!(shown.contains(key.id()));
        let hex = hex(&pkcs8);
        assert!(!shown.contains(&hex[hex.len() - 32..]), "{shown}");
        std::fs::write(dir.path().join("devices").join(KEY_FILE), b"not a key").unwrap();
        assert!(matches!(keys.current(), Err(PushKeyError::Invalid)));
    }

    #[cfg(unix)]
    #[test]
    fn the_key_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let keys = PushKeys::at(dir.path().join("devices"));
        keys.ensure().unwrap();
        let mode = std::fs::metadata(dir.path().join("devices").join(KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
    }
}
