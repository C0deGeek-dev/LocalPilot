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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use localpilot_core::Secret;
use serde::{Deserialize, Serialize};

/// The keychain service name namespacing every stored *provider* credential.
#[cfg(feature = "keychain")]
const SERVICE: &str = "localpilot";

/// The keychain service name namespacing every stored *generic* credential.
///
/// A separate service rather than a prefixed entry name: the keychain is a flat
/// namespace under one service, so a distinct service is what makes a generic
/// credential structurally unable to collide with a provider id — no alias
/// charset has to hold the line.
#[cfg(feature = "keychain")]
const GENERIC_SERVICE: &str = "localpilot-credential";

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
#[derive(Debug, Clone, Default)]
pub struct CredentialStore {
    /// The fallback file location, or `None` when no user dir is resolvable.
    file_path: Option<PathBuf>,
    /// Whether this handle may consult the process user's OS keychain.
    ///
    /// Explicit-file stores are dependency-injected fixtures and must remain
    /// isolated from host state, even when the `keychain` feature is enabled.
    #[cfg(feature = "keychain")]
    use_keychain: bool,
}

impl CredentialStore {
    /// The store rooted at the per-user profile directory.
    #[must_use]
    pub fn user() -> Self {
        Self {
            file_path: crate::load::credential_store_path(),
            #[cfg(feature = "keychain")]
            use_keychain: true,
        }
    }

    /// A file-only store over an explicit path (tests and dependency-injected
    /// callers). `None` disables the file tier. This constructor never consults
    /// the process user's OS keychain, so an isolated caller cannot read or
    /// mutate ambient credentials when the `keychain` feature is enabled.
    #[must_use]
    pub fn with_file(path: Option<PathBuf>) -> Self {
        Self {
            file_path: path,
            #[cfg(feature = "keychain")]
            use_keychain: false,
        }
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
            if self.use_keychain {
                if let Some(secret) = keychain_get(provider_id) {
                    return Some((secret, CredentialSource::Keychain));
                }
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
            if self.use_keychain {
                // A keychain failure (absent/locked) is not fatal: fall through to
                // the file. The error is deliberately not logged — it cannot carry
                // a key, but keeping secrets out of every log path is simplest.
                if keychain_set(provider_id, secret).is_ok() {
                    return Ok(CredentialSource::Keychain);
                }
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
            if self.use_keychain && keychain_delete(provider_id) {
                removed = true;
            }
        }
        if self.file_delete(provider_id)? {
            removed = true;
        }
        Ok(removed)
    }

    /// The stored secret for the generic credential `name`, or `None`.
    ///
    /// Generic credentials share the store's tiers but not its namespace: this
    /// never returns a provider credential, so `credential set openai` and
    /// `login openai` are independent.
    #[must_use]
    pub fn generic_get(&self, name: &str) -> Option<Secret> {
        self.generic_lookup(name).map(|(secret, _)| secret)
    }

    /// Like [`generic_get`](Self::generic_get), but also reports the tier.
    #[must_use]
    pub fn generic_lookup(&self, name: &str) -> Option<(Secret, CredentialSource)> {
        #[cfg(feature = "keychain")]
        {
            if self.use_keychain {
                if let Some(secret) = keychain_get_scoped(GENERIC_SERVICE, name) {
                    return Some((secret, CredentialSource::Keychain));
                }
            }
        }
        let store = self.read_store()?;
        store
            .generic
            .get(name)
            .filter(|value| !value.trim().is_empty())
            .map(|value| (Secret::new(value.clone()), CredentialSource::File))
    }

    /// Store `secret` for the generic credential `name`, preferring the OS
    /// keychain and falling back to the `0600` file. Returns the accepting tier.
    ///
    /// # Errors
    /// [`CredentialError`] when neither tier can store it.
    pub fn generic_set(
        &self,
        name: &str,
        secret: &Secret,
    ) -> Result<CredentialSource, CredentialError> {
        #[cfg(feature = "keychain")]
        {
            if self.use_keychain && keychain_set_scoped(GENERIC_SERVICE, name, secret).is_ok() {
                // Record the name so `generic_list` can see a keychain-backed
                // credential, and drop any stale file-tier copy of the same name
                // so the two tiers cannot disagree.
                self.mutate_store(|store| {
                    store.generic.remove(name);
                    store.generic_keychain.insert(name.to_string());
                })?;
                return Ok(CredentialSource::Keychain);
            }
        }
        self.mutate_store(|store| {
            store.generic_keychain.remove(name);
            // `expose` is the audited exposure point: the file store exists to
            // persist the credential, and the file itself is the secret.
            store
                .generic
                .insert(name.to_string(), secret.expose().to_string());
        })?;
        Ok(CredentialSource::File)
    }

    /// Remove the generic credential `name` from every storage tier. Returns
    /// whether anything was removed.
    ///
    /// # Errors
    /// [`CredentialError::Io`] when rewriting the fallback file fails.
    pub fn generic_delete(&self, name: &str) -> Result<bool, CredentialError> {
        let mut removed = false;
        #[cfg(feature = "keychain")]
        {
            if self.use_keychain && keychain_delete_scoped(GENERIC_SERVICE, name) {
                removed = true;
            }
        }
        if self.file_path.is_some() {
            let mut changed = false;
            self.mutate_store(|store| {
                changed = store.generic.remove(name).is_some();
                changed |= store.generic_keychain.remove(name);
            })?;
            removed |= changed;
        }
        Ok(removed)
    }

    /// Every stored generic credential as `(name, tier)`, sorted by name.
    ///
    /// Reports names and tiers only — never values. There is deliberately no API
    /// that returns a stored value for display, export, or copy.
    #[must_use]
    pub fn generic_list(&self) -> Vec<(String, CredentialSource)> {
        let Some(store) = self.read_store() else {
            return Vec::new();
        };
        let mut entries: Vec<(String, CredentialSource)> = store
            .generic_keychain
            .into_iter()
            .map(|name| (name, CredentialSource::Keychain))
            .chain(
                store
                    .generic
                    .into_keys()
                    .map(|name| (name, CredentialSource::File)),
            )
            .collect();
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        entries
    }

    /// Read the fallback file, or `None` when there is no path or no readable file.
    fn read_store(&self) -> Option<FileStore> {
        read_file_store(self.file_path.as_ref()?)
    }

    /// Apply `change` to the fallback file, creating it when absent.
    fn mutate_store(&self, change: impl FnOnce(&mut FileStore)) -> Result<(), CredentialError> {
        let path = self.file_path.as_ref().ok_or(CredentialError::NoUserDir)?;
        let mut store = read_file_store(path).unwrap_or_default();
        change(&mut store);
        write_file_store(path, &store)
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
///
/// Generic credentials live in their own fields rather than sharing
/// `providers`, so a generic credential and a provider id with the same visible
/// name cannot collide — the separation is structural, not a naming convention
/// something could later forge. Every field is `#[serde(default)]`, so a file
/// written by an earlier version still parses and its providers still resolve.
#[derive(Debug, Default, Serialize, Deserialize)]
struct FileStore {
    #[serde(default)]
    providers: BTreeMap<String, String>,
    /// Generic credentials whose value lives in this file.
    #[serde(default)]
    generic: BTreeMap<String, String>,
    /// Names of generic credentials whose value lives in the OS keychain.
    ///
    /// The keychain exposes no enumeration API, so listing them requires an
    /// index. Only the *name* is recorded here — never the value — which is what
    /// lets `generic_list` report a keychain-backed credential without the file
    /// tier ever holding its secret.
    #[serde(default)]
    generic_keychain: BTreeSet<String>,
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
fn keychain_get(provider_id: &str) -> Option<Secret> {
    keychain_get_scoped(SERVICE, provider_id)
}

#[cfg(feature = "keychain")]
fn keychain_set(provider_id: &str, secret: &Secret) -> Result<(), CredentialError> {
    keychain_set_scoped(SERVICE, provider_id, secret)
}

#[cfg(feature = "keychain")]
fn keychain_delete(provider_id: &str) -> bool {
    keychain_delete_scoped(SERVICE, provider_id)
}

#[cfg(feature = "keychain")]
fn keychain_get_scoped(service: &str, id: &str) -> Option<Secret> {
    let entry = keyring::Entry::new(service, id).ok()?;
    match entry.get_password() {
        Ok(value) if !value.trim().is_empty() => Some(Secret::new(value)),
        _ => None,
    }
}

#[cfg(feature = "keychain")]
fn keychain_set_scoped(service: &str, id: &str, secret: &Secret) -> Result<(), CredentialError> {
    let entry = keyring::Entry::new(service, id)
        .map_err(|error| CredentialError::Keychain(error.to_string()))?;
    entry
        .set_password(secret.expose())
        .map_err(|error| CredentialError::Keychain(error.to_string()))
}

#[cfg(feature = "keychain")]
fn keychain_delete_scoped(service: &str, id: &str) -> bool {
    keyring::Entry::new(service, id)
        .and_then(|entry| entry.delete_credential())
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Explicit-file fixtures are deterministically file-only regardless of
    // whether the `keychain` feature is built. The ignored integration test at
    // the end is the only test in this module allowed to touch host keychain
    // state.
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
    fn explicit_file_stores_are_isolated_from_each_other_and_the_host() {
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        let first = store_at(&first_dir);
        let second = store_at(&second_dir);

        assert_eq!(
            first
                .set("isolated-provider", &Secret::new("provider-secret"))
                .unwrap(),
            CredentialSource::File
        );
        assert_eq!(
            first
                .generic_set("isolated-generic", &Secret::new("generic-secret"))
                .unwrap(),
            CredentialSource::File
        );
        assert_eq!(
            first.source("isolated-provider"),
            Some(CredentialSource::File)
        );
        assert_eq!(
            first
                .generic_lookup("isolated-generic")
                .map(|(_, source)| source),
            Some(CredentialSource::File)
        );

        // A second injected store cannot observe either value through ambient
        // keychain state, even in an all-features build on a desktop host.
        assert!(second.get("isolated-provider").is_none());
        assert!(second.generic_get("isolated-generic").is_none());

        let persisted = std::fs::read_to_string(first_dir.path().join("credentials.json"))
            .expect("the isolated store writes its explicit file");
        assert!(persisted.contains("provider-secret"));
        assert!(persisted.contains("generic-secret"));

        assert!(first.delete("isolated-provider").unwrap());
        assert!(first.generic_delete("isolated-generic").unwrap());
        assert!(first.get("isolated-provider").is_none());
        assert!(first.generic_get("isolated-generic").is_none());
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

    /// A credential file written before generic credentials existed carries only
    /// `providers`. It must still parse and still resolve — the generic fields
    /// are additive, so an upgrade never costs a user their stored logins.
    #[test]
    fn a_credential_file_from_an_earlier_version_still_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        std::fs::write(&path, r#"{"providers":{"anthropic":"sk-existing-value"}}"#).unwrap();
        let store = CredentialStore::with_file(Some(path));

        assert_eq!(
            store.file_get("anthropic").map(|s| s.expose().to_string()),
            Some("sk-existing-value".to_string())
        );
        // No generic credentials, and reading them does not disturb the providers.
        assert!(store.generic_list().is_empty());
        store
            .generic_set("added-later", &Secret::new("generic-value"))
            .unwrap();
        assert_eq!(
            store.file_get("anthropic").map(|s| s.expose().to_string()),
            Some("sk-existing-value".to_string())
        );
    }

    /// The file tier keeps generic credentials in their own map, so a generic
    /// name and a provider id may be identical without either being reachable
    /// through the other's accessors.
    #[test]
    fn generic_and_provider_namespaces_are_separate_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_at(&dir);
        store
            .file_set("openai", &Secret::new("provider-value"))
            .unwrap();
        store
            .generic_set("openai", &Secret::new("generic-value"))
            .unwrap();

        assert_eq!(
            store.file_get("openai").map(|s| s.expose().to_string()),
            Some("provider-value".to_string())
        );
        assert_eq!(
            store.generic_get("openai").map(|s| s.expose().to_string()),
            Some("generic-value".to_string())
        );
        // `generic_list` reports the generic entry only, with its tier.
        assert_eq!(
            store.generic_list(),
            vec![("openai".to_string(), CredentialSource::File)]
        );
        // Removing the generic entry leaves the provider credential intact.
        assert!(store.generic_delete("openai").unwrap());
        assert!(store.generic_get("openai").is_none());
        assert_eq!(
            store.file_get("openai").map(|s| s.expose().to_string()),
            Some("provider-value".to_string())
        );
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
