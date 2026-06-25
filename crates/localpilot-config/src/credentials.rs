//! Stored API credentials.
//!
//! A best-effort OS-keychain store with a restrictive-mode file fallback, plus
//! the *source* a resolved credential came from for diagnostics. The keychain
//! backend is built only with the `keychain` Cargo feature and currently covers
//! the Windows Credential Manager (the macOS/Linux native backends are held back
//! by an MSRV constraint — see ADR-0042). Without the feature, on macOS/Linux, or
//! on any host whose keychain is absent or locked, the store falls back to a
//! `0600` file under the per-user profile directory, and resolution still falls
//! through to the environment — so a missing keychain never blocks startup or a
//! session.
//!
//! Secret discipline: a credential never appears in logs, errors, `Debug` output,
//! transcripts, or config. The value leaves the [`Secret`] wrapper only at the
//! audited keychain/file write calls in this module, whose sole purpose is to
//! persist it; the file is owner-only on unix and lives in the user profile.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use localpilot_core::Secret;
use serde::{Deserialize, Serialize};

/// The keychain service name namespacing every stored credential.
#[cfg(feature = "keychain")]
const SERVICE: &str = "localpilot";

/// Which tier a resolved credential came from. Reported by `doctor`; it never
/// carries the value itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    /// The OS keychain (Credential Manager / Keychain / Secret Service).
    Keychain,
    /// The restrictive-mode fallback file under the user profile directory.
    File,
    /// An environment variable (`api_key_env` or a provider-kind default).
    Env,
    /// Google Application Default Credentials from the standard ADC search path.
    GoogleAdc,
    /// Google Application Default Credentials from an explicitly configured file.
    GoogleAdcFile,
    /// No credential is available from any source.
    None,
}

impl CredentialSource {
    /// A short, secret-free label for diagnostics.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            CredentialSource::Keychain => "keychain",
            CredentialSource::File => "file",
            CredentialSource::Env => "env",
            CredentialSource::GoogleAdc => "google_adc",
            CredentialSource::GoogleAdcFile => "google_adc_file",
            CredentialSource::None => "none",
        }
    }
}

/// A failure storing or removing a credential. Never carries the secret value.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    /// No per-user profile directory is available for the fallback file.
    #[error("no user profile directory is available to store a credential")]
    NoUserDir,
    /// A filesystem operation on the fallback file failed.
    #[error("credential file error: {0}")]
    Io(String),
    /// The OS keychain rejected the operation.
    #[error("keychain error: {0}")]
    Keychain(String),
}

/// A handle to the credential store: the OS keychain (best-effort) over a
/// restrictive-mode file fallback.
#[derive(Debug, Clone)]
pub struct CredentialStore {
    /// The fallback file location, or `None` when no user dir is resolvable.
    file_path: Option<PathBuf>,
}

impl CredentialStore {
    /// The store rooted at the per-user profile directory.
    #[must_use]
    pub fn user() -> Self {
        Self {
            file_path: crate::load::credential_store_path(),
        }
    }

    /// A store over an explicit fallback-file path (tests and callers that resolve
    /// their own location). `None` disables the file tier.
    #[must_use]
    pub fn with_file(path: Option<PathBuf>) -> Self {
        Self { file_path: path }
    }

    /// The stored secret for `provider_id`, or `None` for a clean miss. The OS
    /// keychain is consulted first (when built with the `keychain` feature and a
    /// backend is present), then the fallback file. A keychain that is absent or
    /// locked is a miss, never an error.
    #[must_use]
    pub fn get(&self, provider_id: &str) -> Option<Secret> {
        self.lookup(provider_id).map(|(secret, _)| secret)
    }

    /// Like [`get`](Self::get), but also reports which tier held the secret.
    #[must_use]
    pub fn lookup(&self, provider_id: &str) -> Option<(Secret, CredentialSource)> {
        #[cfg(feature = "keychain")]
        {
            if let Some(secret) = keychain_get(provider_id) {
                return Some((secret, CredentialSource::Keychain));
            }
        }
        self.file_get(provider_id)
            .map(|secret| (secret, CredentialSource::File))
    }

    /// Whether a credential is stored for `provider_id` in any tier, without
    /// returning the value.
    #[must_use]
    pub fn source(&self, provider_id: &str) -> Option<CredentialSource> {
        self.lookup(provider_id).map(|(_, source)| source)
    }

    /// Store `secret` for `provider_id`, preferring the OS keychain and falling
    /// back to the `0600` file. Returns the tier that accepted it.
    ///
    /// # Errors
    /// [`CredentialError`] when neither the keychain nor the file can store it
    /// (no user dir, an I/O failure, or a keychain rejection with no usable
    /// fallback path).
    pub fn set(
        &self,
        provider_id: &str,
        secret: &Secret,
    ) -> Result<CredentialSource, CredentialError> {
        #[cfg(feature = "keychain")]
        {
            // A keychain failure (absent/locked) is not fatal: fall through to the
            // file. The error is deliberately not logged — it cannot carry a key,
            // but keeping secrets out of every log path is the simplest guarantee.
            if keychain_set(provider_id, secret).is_ok() {
                return Ok(CredentialSource::Keychain);
            }
        }
        self.file_set(provider_id, secret)?;
        Ok(CredentialSource::File)
    }

    /// Remove any stored credential for `provider_id` from every tier. Returns
    /// whether anything was removed.
    ///
    /// # Errors
    /// [`CredentialError::Io`] when rewriting the fallback file fails.
    pub fn delete(&self, provider_id: &str) -> Result<bool, CredentialError> {
        let mut removed = false;
        #[cfg(feature = "keychain")]
        {
            if keychain_delete(provider_id) {
                removed = true;
            }
        }
        if self.file_delete(provider_id)? {
            removed = true;
        }
        Ok(removed)
    }

    fn file_get(&self, provider_id: &str) -> Option<Secret> {
        let path = self.file_path.as_ref()?;
        let store = read_file_store(path)?;
        store
            .providers
            .get(provider_id)
            .filter(|value| !value.trim().is_empty())
            .map(|value| Secret::new(value.clone()))
    }

    pub(crate) fn file_set(
        &self,
        provider_id: &str,
        secret: &Secret,
    ) -> Result<(), CredentialError> {
        let path = self.file_path.as_ref().ok_or(CredentialError::NoUserDir)?;
        let mut store = read_file_store(path).unwrap_or_default();
        // `expose` is the audited exposure point: the file store exists to persist
        // the credential. The file itself is the secret, protected by owner-only
        // mode (unix) and the user-profile location.
        store
            .providers
            .insert(provider_id.to_string(), secret.expose().to_string());
        write_file_store(path, &store)
    }

    fn file_delete(&self, provider_id: &str) -> Result<bool, CredentialError> {
        let Some(path) = self.file_path.as_ref() else {
            return Ok(false);
        };
        let Some(mut store) = read_file_store(path) else {
            return Ok(false);
        };
        let removed = store.providers.remove(provider_id).is_some();
        if removed {
            write_file_store(path, &store)?;
        }
        Ok(removed)
    }
}

/// The on-disk fallback store: a flat map of provider id to credential. Stored
/// raw (it is the secret), protected by file mode and location, never redacted.
#[derive(Debug, Default, Serialize, Deserialize)]
struct FileStore {
    #[serde(default)]
    providers: BTreeMap<String, String>,
}

/// Read and parse the fallback file, or `None` when it is missing or unreadable.
fn read_file_store(path: &Path) -> Option<FileStore> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Write the fallback file with owner-only permissions, creating the parent dir.
fn write_file_store(path: &Path, store: &FileStore) -> Result<(), CredentialError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| CredentialError::Io(error.to_string()))?;
    }
    let body =
        serde_json::to_vec_pretty(store).map_err(|error| CredentialError::Io(error.to_string()))?;
    write_owner_only(path, &body)
}

/// Write `body` to `path` so only the owner can read it. On unix the file is
/// created at mode `0600` (and re-asserted, in case it pre-existed looser); other
/// platforms rely on the per-user profile directory's own ACL — tier-1 parity is
/// behaviour parity, the FS permission mechanism differs by platform.
#[cfg(unix)]
fn write_owner_only(path: &Path, body: &[u8]) -> Result<(), CredentialError> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| CredentialError::Io(error.to_string()))?;
    file.write_all(body)
        .map_err(|error| CredentialError::Io(error.to_string()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| CredentialError::Io(error.to_string()))
}

#[cfg(not(unix))]
fn write_owner_only(path: &Path, body: &[u8]) -> Result<(), CredentialError> {
    std::fs::write(path, body).map_err(|error| CredentialError::Io(error.to_string()))
}

#[cfg(feature = "keychain")]
fn keychain_entry(provider_id: &str) -> Result<keyring::Entry, keyring::Error> {
    keyring::Entry::new(SERVICE, provider_id)
}

#[cfg(feature = "keychain")]
fn keychain_get(provider_id: &str) -> Option<Secret> {
    let entry = keychain_entry(provider_id).ok()?;
    match entry.get_password() {
        Ok(value) if !value.trim().is_empty() => Some(Secret::new(value)),
        _ => None,
    }
}

#[cfg(feature = "keychain")]
fn keychain_set(provider_id: &str, secret: &Secret) -> Result<(), CredentialError> {
    let entry = keychain_entry(provider_id)
        .map_err(|error| CredentialError::Keychain(error.to_string()))?;
    entry
        .set_password(secret.expose())
        .map_err(|error| CredentialError::Keychain(error.to_string()))
}

#[cfg(feature = "keychain")]
fn keychain_delete(provider_id: &str) -> bool {
    keychain_entry(provider_id)
        .and_then(|entry| entry.delete_credential())
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The file-tier tests drive `file_*` directly so they are deterministic
    // regardless of whether the `keychain` feature is built: the public
    // `get`/`set`/`delete` consult the real OS keychain first (covered by the
    // ignored keychain test), which is shared host state unfit for assertions.
    fn store_at(dir: &tempfile::TempDir) -> CredentialStore {
        CredentialStore::with_file(Some(dir.path().join("credentials.json")))
    }

    #[test]
    fn an_absent_file_tier_is_a_clean_miss_not_an_error() {
        // The headless story: with no file (and no keychain on the default build)
        // a lookup yields None, never a failure — resolution then falls through to
        // the environment.
        let empty = CredentialStore::with_file(None);
        assert!(empty.file_get("anthropic").is_none());

        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir);
        assert!(store.file_get("anthropic").is_none());
    }

    #[test]
    fn a_stored_credential_round_trips_through_the_file_tier() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir);
        store
            .file_set("anthropic", &Secret::new("sk-test-value"))
            .unwrap();

        let secret = store.file_get("anthropic").expect("credential round-trips");
        assert_eq!(secret.expose(), "sk-test-value");
        // The returned secret stays redacted in formatting output.
        assert_eq!(format!("{secret}"), "***");
        assert!(!format!("{secret:?}").contains("sk-test-value"));
        // The lookup tier for a file-stored credential is reported as `File`.
        assert_eq!(
            store.lookup("anthropic").map(|(_, source)| source),
            Some(CredentialSource::File)
        );

        // Deleting removes it from the store.
        assert!(store.file_delete("anthropic").unwrap());
        assert!(store.file_get("anthropic").is_none());
        // A second delete is a clean `false`, not an error.
        assert!(!store.file_delete("anthropic").unwrap());
    }

    #[test]
    fn separate_providers_have_independent_file_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir);
        store.file_set("anthropic", &Secret::new("a-key")).unwrap();
        store.file_set("openai", &Secret::new("o-key")).unwrap();
        assert_eq!(store.file_get("anthropic").unwrap().expose(), "a-key");
        assert_eq!(store.file_get("openai").unwrap().expose(), "o-key");
        store.file_delete("anthropic").unwrap();
        assert!(store.file_get("anthropic").is_none());
        // Deleting one leaves the other intact.
        assert_eq!(store.file_get("openai").unwrap().expose(), "o-key");
    }

    #[cfg(unix)]
    #[test]
    fn the_fallback_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let store = CredentialStore::with_file(Some(path.clone()));
        store
            .file_set("anthropic", &Secret::new("sk-secret"))
            .unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "credential file must be owner-only");
        // The raw secret is on disk (the file is the secret) but never in any log
        // or formatted form; the file's protection is its mode and location.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("sk-secret"));
    }

    // The real OS keychain is exercised only when explicitly opted in: it touches
    // the host's credential service, which is absent on CI and headless Linux, so
    // it is both feature- and `#[ignore]`-gated to keep the default run green.
    #[cfg(feature = "keychain")]
    #[test]
    #[ignore = "touches the real OS keychain; run with --ignored on a desktop"]
    fn keychain_round_trips_on_a_real_backend() {
        let store = CredentialStore::user();
        let provider = "localpilot-test-provider";
        store.set(provider, &Secret::new("sk-keychain")).unwrap();
        assert_eq!(store.get(provider).unwrap().expose(), "sk-keychain");
        assert_eq!(store.source(provider), Some(CredentialSource::Keychain));
        store.delete(provider).unwrap();
        assert!(store.get(provider).is_none());
    }
}
