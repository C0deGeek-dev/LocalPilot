//! Resolving a configured MCP server's environment, and spawning it with that
//! environment applied.
//!
//! This is the single entry point every MCP launch path goes through — the
//! interactive session's server discovery, designated research search tools, and
//! `doctor`'s connectivity probe. They used to call `StdioTransport::spawn`
//! directly, which is exactly how three copies of a policy drift apart; keeping
//! one seam means a change to what an entry form means, or to what a missing
//! credential does, cannot land in one path and miss the others.
//!
//! Resolution never leaves a credential unwrapped: a value from the credential
//! store or an explicit sensitive literal becomes a [`Secret`] here and stays one
//! until the audited assignment inside the transport.

use std::sync::Arc;

use localpilot_config::{CredentialStore, McpEnvEntry, McpServerConfig};
use localpilot_core::Secret;
use localpilot_mcp::{McpError, ResolvedEnvEntry, ServerEnvironment, StdioTransport, Transport};

/// Why a server's environment could not be resolved.
///
/// Carries the variable and alias names so the message can be actionable, and
/// never a value — this type reaches `doctor` output and logs.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvResolutionError {
    /// A `{ credential = "..." }` entry names a credential that is not stored.
    #[error(
        "environment variable {variable} needs the credential {alias:?}, which is not stored \
         (add it with `localpilot credential set {alias}`)"
    )]
    MissingCredential {
        /// The environment variable the entry would have set.
        variable: String,
        /// The credential alias that could not be resolved.
        alias: String,
    },
}

/// Resolve a server's configured `env` into a concrete overlay.
///
/// A plain string resolves as a non-sensitive value; `{ value = "..." }` as a
/// sensitive literal; `{ credential = "alias" }` from the generic credential
/// store, also sensitive. A credential reference that does not resolve is an
/// error for this server rather than a silently missing variable — starting a
/// server without the credential it was configured to need only moves the
/// failure somewhere harder to read.
///
/// # Errors
/// [`EnvResolutionError::MissingCredential`] when a referenced credential is
/// absent from every storage tier.
pub fn resolve_environment(
    server: &McpServerConfig,
    store: &CredentialStore,
) -> Result<ServerEnvironment, EnvResolutionError> {
    let mut entries = Vec::with_capacity(server.env.len());
    for (name, entry) in &server.env {
        let resolved = match entry {
            McpEnvEntry::Plain(value) => ResolvedEnvEntry {
                name: name.clone(),
                value: Secret::new(value.clone()),
                sensitive: false,
            },
            McpEnvEntry::Object(object) => {
                if let Some(literal) = &object.value {
                    ResolvedEnvEntry {
                        name: name.clone(),
                        value: literal.secret().clone(),
                        sensitive: true,
                    }
                } else if let Some(alias) = &object.credential {
                    let value = store.generic_get(alias).ok_or_else(|| {
                        EnvResolutionError::MissingCredential {
                            variable: name.clone(),
                            alias: alias.clone(),
                        }
                    })?;
                    ResolvedEnvEntry {
                        name: name.clone(),
                        value,
                        sensitive: true,
                    }
                } else {
                    // Config validation rejects an object setting neither form,
                    // so this is unreachable through `load`. Treat it as an empty
                    // non-sensitive value rather than panicking on a library path.
                    ResolvedEnvEntry {
                        name: name.clone(),
                        value: Secret::new(String::new()),
                        sensitive: false,
                    }
                }
            }
        };
        entries.push(resolved);
    }
    Ok(ServerEnvironment::new(entries))
}

/// Why a server could not be started.
#[derive(Debug, thiserror::Error)]
pub enum ServerLaunchError {
    /// The environment could not be resolved; no process was spawned.
    #[error(transparent)]
    Environment(#[from] EnvResolutionError),
    /// The process could not be started or spoke no usable stdio.
    #[error(transparent)]
    Transport(#[from] McpError),
}

/// Resolve `server`'s environment and spawn it with that overlay applied.
///
/// Resolution happens first and completely: a server whose credential is missing
/// is never spawned, so a misconfigured server fails as a configuration problem
/// rather than as an obscure runtime error from a process that started without
/// the value it needed.
///
/// # Errors
/// [`ServerLaunchError`] when the environment cannot be resolved or the process
/// cannot be started.
pub fn spawn_server(
    server: &McpServerConfig,
    store: &CredentialStore,
) -> Result<(Arc<dyn Transport>, ServerEnvironment), ServerLaunchError> {
    let environment = resolve_environment(server, store)?;
    let transport = StdioTransport::spawn(&server.command, &server.args, &environment)?;
    Ok((Arc::new(transport) as Arc<dyn Transport>, environment))
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_config::{McpEnvObject, SensitiveLiteral};

    fn store_at(dir: &tempfile::TempDir) -> CredentialStore {
        CredentialStore::with_file(Some(dir.path().join("credentials.json")))
    }

    fn server_with(env: Vec<(&str, McpEnvEntry)>) -> McpServerConfig {
        McpServerConfig {
            command: "does-not-matter".to_string(),
            args: Vec::new(),
            env: env
                .into_iter()
                .map(|(name, entry)| (name.to_string(), entry))
                .collect(),
        }
    }

    fn credential(alias: &str) -> McpEnvEntry {
        McpEnvEntry::Object(McpEnvObject {
            value: None,
            credential: Some(alias.to_string()),
        })
    }

    fn literal(value: &str) -> McpEnvEntry {
        McpEnvEntry::Object(McpEnvObject {
            value: Some(SensitiveLiteral::new(value)),
            credential: None,
        })
    }

    #[test]
    fn each_entry_form_resolves_with_the_right_sensitivity() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = store_at(&dir);
        store.generic_set("my-alias", &Secret::new("credential-value"))?;

        let server = server_with(vec![
            ("LOG_LEVEL", McpEnvEntry::Plain("info".to_string())),
            ("SERVICE_TOKEN", literal("literal-value")),
            ("SERVICE_KEY", credential("my-alias")),
        ]);
        let resolved = resolve_environment(&server, &store)?;

        assert_eq!(
            resolved.names().collect::<Vec<_>>(),
            vec!["LOG_LEVEL", "SERVICE_TOKEN", "SERVICE_KEY"]
        );
        // Only the two credential-bearing entries feed the redaction pass; a
        // plain value is not worth an exact-match needle.
        let mut secrets: Vec<_> = resolved.secrets().map(|s| s.expose().to_string()).collect();
        secrets.sort();
        assert_eq!(secrets, vec!["credential-value", "literal-value"]);
        Ok(())
    }

    #[test]
    fn a_missing_credential_is_an_error_naming_the_variable_and_alias() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = store_at(&dir);
        let server = server_with(vec![("SERVICE_KEY", credential("absent-alias"))]);

        let error = resolve_environment(&server, &store)
            .expect_err("an unresolvable credential should fail resolution");
        assert_eq!(
            error,
            EnvResolutionError::MissingCredential {
                variable: "SERVICE_KEY".to_string(),
                alias: "absent-alias".to_string(),
            }
        );
        // The message points at the fix without echoing any value.
        let message = error.to_string();
        assert!(message.contains("SERVICE_KEY"));
        assert!(message.contains("localpilot credential set absent-alias"));
        Ok(())
    }

    #[test]
    fn a_server_with_no_configured_environment_resolves_to_an_empty_overlay() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let resolved = resolve_environment(&server_with(Vec::new()), &store_at(&dir))?;
        assert!(resolved.is_empty());
        assert_eq!(resolved.len(), 0);
        assert_eq!(resolved.secrets().count(), 0);
        Ok(())
    }

    /// A resolved overlay must not surface a value through `Debug`, which is how
    /// it would reach a log line or an error context.
    #[test]
    fn debug_of_a_resolved_overlay_hides_every_value() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = store_at(&dir);
        store.generic_set("my-alias", &Secret::new("credential-value"))?;
        let server = server_with(vec![
            ("SERVICE_KEY", credential("my-alias")),
            ("SERVICE_TOKEN", literal("literal-value")),
            ("LOG_LEVEL", McpEnvEntry::Plain("info".to_string())),
        ]);

        let rendered = format!("{:?}", resolve_environment(&server, &store)?);
        assert!(!rendered.contains("credential-value"));
        assert!(!rendered.contains("literal-value"));
        // Even a non-sensitive value stays wrapped, so nothing leaks by default.
        assert!(!rendered.contains("info"));
        // Names are safe and still visible, so the render stays useful.
        assert!(rendered.contains("SERVICE_KEY"));
        Ok(())
    }
}
