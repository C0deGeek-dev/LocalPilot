//! `localpilot credential set|list|delete` — the provider-neutral credential flow.
//!
//! These credentials are not provider API keys: they are named values a
//! configured MCP server needs in its environment, referenced from
//! `[mcp.servers.<name>.env]` as `{ credential = "<name>" }`. They share the
//! store's tiers (OS keychain, then a `0600` fallback file) with provider
//! credentials but not its namespace, so `credential set openai` cannot
//! overwrite the key `localpilot login openai` stored.
//!
//! Secret discipline matches the login flow: the value is read from stdin,
//! wrapped in [`Secret`] immediately, never accepted as a command-line argument,
//! and never printed in full. There is deliberately no command to show, export,
//! or copy a stored value — once set, a credential is write-only from the CLI's
//! point of view.

use std::io::{self, BufRead, Write};

use anyhow::{anyhow, Context};
use localpilot_config::{is_portable_credential_alias, CredentialStore};
use localpilot_core::Secret;

use crate::login_cmd::mask;

/// Run `localpilot credential set <name>`: read one value from stdin and store it.
///
/// # Errors
/// Returns an error if the name is invalid, no value is entered, or the store
/// rejects the write.
pub fn set(name: &str) -> anyhow::Result<()> {
    let stdin = io::stdin();
    set_with(
        name,
        &CredentialStore::user(),
        &mut stdin.lock(),
        &mut io::stdout(),
    )
}

/// Run `localpilot credential list`: report stored names and their tiers.
///
/// # Errors
/// Returns an error only if writing to the output stream fails.
pub fn list() -> anyhow::Result<()> {
    list_with(&CredentialStore::user(), &mut io::stdout())
}

/// Run `localpilot credential delete <name>`: remove it from every tier.
///
/// # Errors
/// Returns an error if the name is invalid or the store rejects the removal.
pub fn delete(name: &str) -> anyhow::Result<()> {
    delete_with(name, &CredentialStore::user(), &mut io::stdout())
}

/// The read-and-store half of `set`, with the store and streams injected so the
/// flow is testable without touching the real per-user store or a live keychain.
fn set_with(
    name: &str,
    store: &CredentialStore,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    validate_name(name)?;
    write!(
        out,
        "Paste the value for {name:?} and press Enter (stored secret, shown masked): "
    )?;
    out.flush()?;
    let mut line = String::new();
    input
        .read_line(&mut line)
        .context("reading the credential value")?;
    let secret = Secret::new(line.trim().to_string());
    if secret.is_empty() {
        return Err(anyhow!("no value entered; nothing stored"));
    }

    let source = store
        .generic_set(name, &secret)
        .with_context(|| format!("storing the {name:?} credential"))?;
    writeln!(
        out,
        "stored credential {name:?} in the {} ({})",
        source.label(),
        mask(secret.expose())
    )?;
    Ok(())
}

/// The report half of `list`, with the store injected. Names and tiers only.
fn list_with(store: &CredentialStore, out: &mut dyn Write) -> anyhow::Result<()> {
    let entries = store.generic_list();
    if entries.is_empty() {
        writeln!(out, "no stored credentials")?;
        return Ok(());
    }
    for (name, source) in entries {
        writeln!(out, "{name}\t{}", source.label())?;
    }
    Ok(())
}

/// The delete-and-report half of `delete`, with the store injected.
fn delete_with(name: &str, store: &CredentialStore, out: &mut dyn Write) -> anyhow::Result<()> {
    validate_name(name)?;
    if store
        .generic_delete(name)
        .with_context(|| format!("removing the {name:?} credential"))?
    {
        writeln!(out, "removed credential {name:?}")?;
    } else {
        writeln!(out, "no stored credential {name:?} to remove")?;
    }
    Ok(())
}

/// Reject a name config would not accept, so a credential cannot be stored under
/// an alias no `[mcp.servers.<name>.env]` entry could ever reference.
fn validate_name(name: &str) -> anyhow::Result<()> {
    if is_portable_credential_alias(name) {
        return Ok(());
    }
    Err(anyhow!(
        "invalid credential name {name:?}: use letters, digits, \".\", \"-\", or \"_\""
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_at(dir: &tempfile::TempDir) -> CredentialStore {
        CredentialStore::with_file(Some(dir.path().join("credentials.json")))
    }

    fn output(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    #[test]
    fn set_list_and_delete_round_trip_without_exposing_the_value() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = store_at(&dir);
        let secret = "super-secret-credential-value";

        let mut out = Vec::new();
        set_with(
            "google-api-key",
            &store,
            &mut format!("{secret}\n").as_bytes(),
            &mut out,
        )?;
        let set_output = output(&out);
        assert!(set_output.contains("google-api-key"));
        assert!(
            !set_output.contains(secret),
            "set output must not echo the value: {set_output}"
        );

        // The value round-trips through the store even though no command shows it.
        assert_eq!(
            store
                .generic_get("google-api-key")
                .map(|s| s.expose().to_string()),
            Some(secret.to_string())
        );

        let mut out = Vec::new();
        list_with(&store, &mut out)?;
        let listed = output(&out);
        assert!(listed.contains("google-api-key"));
        assert!(listed.contains("file"));
        assert!(
            !listed.contains(secret),
            "list must report names and tiers only: {listed}"
        );

        let mut out = Vec::new();
        delete_with("google-api-key", &store, &mut out)?;
        assert!(output(&out).contains("removed"));
        assert!(store.generic_get("google-api-key").is_none());

        // Deleting again reports the miss rather than failing.
        let mut out = Vec::new();
        delete_with("google-api-key", &store, &mut out)?;
        assert!(output(&out).contains("no stored credential"));
        Ok(())
    }

    /// The collision the generic namespace exists to prevent: a generic
    /// credential and a provider credential may share a visible name and must
    /// remain completely independent.
    #[test]
    fn a_generic_credential_never_collides_with_a_provider_credential() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = store_at(&dir);
        let provider_key = Secret::new("provider-login-key");

        store.set("openai", &provider_key)?;
        set_with(
            "openai",
            &store,
            &mut "generic-credential-value\n".as_bytes(),
            &mut Vec::new(),
        )?;

        // Both resolve, independently, under the same visible name.
        assert_eq!(
            store.get("openai").map(|s| s.expose().to_string()),
            Some("provider-login-key".to_string())
        );
        assert_eq!(
            store.generic_get("openai").map(|s| s.expose().to_string()),
            Some("generic-credential-value".to_string())
        );

        // Removing the generic one leaves the provider login untouched.
        assert!(store.generic_delete("openai")?);
        assert_eq!(
            store.get("openai").map(|s| s.expose().to_string()),
            Some("provider-login-key".to_string())
        );
        assert!(store.generic_get("openai").is_none());
        Ok(())
    }

    #[test]
    fn an_empty_value_stores_nothing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = store_at(&dir);
        let error = set_with("alias", &store, &mut "\n".as_bytes(), &mut Vec::new())
            .expect_err("an empty value should be refused");
        assert!(error.to_string().contains("no value entered"));
        assert!(store.generic_get("alias").is_none());
        Ok(())
    }

    #[test]
    fn an_invalid_name_is_refused_by_set_and_delete() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_at(&dir);
        for name in ["has space", "has/slash", ""] {
            assert!(set_with(name, &store, &mut "value\n".as_bytes(), &mut Vec::new()).is_err());
            assert!(delete_with(name, &store, &mut Vec::new()).is_err());
        }
    }

    #[test]
    fn listing_an_empty_store_says_so() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut out = Vec::new();
        list_with(&store_at(&dir), &mut out)?;
        assert!(output(&out).contains("no stored credentials"));
        Ok(())
    }
}
