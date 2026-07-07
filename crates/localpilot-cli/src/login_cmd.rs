//! `localpilot login` / `logout` — the BYOK credential flow.
//!
//! A user creates an API key in the provider's own dashboard (we can deep-link
//! there) and pastes it; we validate it with one minimal request and store it in
//! the OS keychain (the Windows Credential Manager, when built with the `keychain`
//! feature) or a restrictive-mode fallback file (macOS/Linux, and any host without
//! a keychain backend) — see ADR-0042. This is the only
//! sanctioned credential path: there is no provider-blessed OAuth flow that mints
//! a standard API key for a third-party client, and routing a third party's users
//! through Claude/ChatGPT *subscription* credentials is prohibited. So this flow
//! is strictly bring-your-own-key — no "sign in with Claude/ChatGPT", no
//! subscription tokens.
//!
//! Secret discipline: the pasted key is wrapped in [`Secret`] immediately, never
//! logged, and only ever echoed back masked.

use std::io::{self, Write};

use anyhow::{anyhow, Context};
use localpilot_config::{CliOverrides, Config, ConfigPaths, CredentialStore};
use localpilot_core::Secret;

/// Options for the login flow.
#[derive(Debug, Clone, Copy, Default)]
pub struct LoginOptions {
    /// Do not open the browser at the key-creation page (still printed).
    pub no_browser: bool,
    /// Skip the validation request (store without checking the key works).
    pub no_verify: bool,
}

/// A supported BYOK provider: where to create a key, and how to validate one.
struct ProviderInfo {
    /// The credential account id (the configured provider id the key resolves
    /// under), e.g. `anthropic`.
    id: String,
    /// Human-facing provider name.
    display: String,
    /// The provider's official key-creation page (the deep-link target).
    key_url: String,
    /// How a minimal validation request is shaped for this provider.
    wire: Wire,
    /// Base URL for the validation request.
    base_url: String,
}

/// The two supported credential wires for validation. Each issues the documented
/// public model-listing request with the provider's own auth header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wire {
    /// `Authorization: Bearer <key>` against an OpenAI-compatible `/models`.
    OpenAi,
    /// `x-api-key: <key>` + `anthropic-version` against Anthropic's `/v1/models`.
    Anthropic,
}

const ANTHROPIC_KEY_URL: &str = "https://console.anthropic.com/settings/keys";
const OPENAI_KEY_URL: &str = "https://platform.openai.com/api-keys";
const ANTHROPIC_DEFAULT_BASE_URL: &str = "https://api.anthropic.com/v1";
const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Run `localpilot login <provider>`: deep-link, prompt, validate, store.
///
/// # Errors
/// Returns an error if config cannot load, the provider is unsupported, no key is
/// entered, or the store rejects the write.
pub async fn login(provider: &str, options: LoginOptions) -> anyhow::Result<()> {
    let config = load_config();
    let info = resolve_provider(provider, config.as_ref())?;
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let key = read_pasted_key(&info, options, &stdin, &mut stdout)?;
    let store = CredentialStore::user();
    login_with(&info, options, &key, &store, &mut stdout).await
}

/// Run `localpilot logout <provider>`: remove any stored credential.
///
/// # Errors
/// Returns an error if the store rejects the delete.
pub fn logout(provider: &str) -> anyhow::Result<()> {
    let config = load_config();
    let info = resolve_provider(provider, config.as_ref())?;
    let store = CredentialStore::user();
    let mut stdout = io::stdout();
    logout_with(&info.id, &store, &mut stdout)
}

/// The store-and-report half of login, with I/O and the store injected so it is
/// testable offline without touching the real keychain or user directory.
async fn login_with(
    info: &ProviderInfo,
    options: LoginOptions,
    key: &Secret,
    store: &CredentialStore,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    if key.expose().trim().is_empty() {
        return Err(anyhow!("no key entered; nothing stored"));
    }

    // Validate with one minimal request unless skipped. The two failure modes
    // differ: a key the provider actively REJECTED is not stored (persisting
    // it would seed every later session with a known-bad credential) — the
    // explicit `--no-verify` override stores it anyway and says so. A network
    // or endpoint failure stays non-fatal: an offline setup or an
    // unlisted-but-valid key is never blocked.
    if !options.no_verify {
        match validate(info, key).await {
            Ok(true) => writeln!(out, "key accepted by {}", info.display)?,
            Ok(false) => {
                return Err(anyhow!(
                    "{} rejected the key; nothing stored (re-run with --no-verify to store it anyway)",
                    info.display
                ));
            }
            Err(error) => {
                writeln!(
                    out,
                    "warning: could not validate the key ({error}); storing it anyway"
                )?;
            }
        }
    }

    let source = store
        .set(&info.id, key)
        .with_context(|| format!("storing the {} credential", info.id))?;
    writeln!(
        out,
        "stored {} credential in the {} ({})",
        info.id,
        source.label(),
        mask(key.expose())
    )?;
    Ok(())
}

/// The delete-and-report half of logout, with the store injected for tests.
fn logout_with(id: &str, store: &CredentialStore, out: &mut dyn Write) -> anyhow::Result<()> {
    if store
        .delete(id)
        .with_context(|| format!("removing the {id} credential"))?
    {
        writeln!(out, "removed the stored {id} credential")?;
    } else {
        writeln!(out, "no stored {id} credential to remove")?;
    }
    Ok(())
}

/// Deep-link (best-effort), then read the pasted key from stdin. The key is
/// wrapped immediately; it is never logged and only ever echoed masked.
fn read_pasted_key(
    info: &ProviderInfo,
    options: LoginOptions,
    stdin: &io::Stdin,
    out: &mut dyn Write,
) -> anyhow::Result<Secret> {
    writeln!(out, "Create or copy a {} API key here:", info.display)?;
    writeln!(out, "  {}", info.key_url)?;
    if !options.no_browser {
        // Opening the browser is a convenience, never required: the URL is always
        // printed above, so a headless host completes the flow by paste alone.
        open_browser(&info.key_url);
    }
    write!(
        out,
        "Paste the key and press Enter (input is stored secret, shown masked): "
    )?;
    out.flush()?;
    let mut line = String::new();
    stdin
        .read_line(&mut line)
        .context("reading the pasted key")?;
    Ok(Secret::new(line.trim().to_string()))
}

/// Resolve the provider argument to its BYOK info: a configured provider id maps
/// through its `kind`; the bare kind names `anthropic` / `openai` also work for a
/// first-time login with no config. A configured `base_url` overrides the default.
fn resolve_provider(provider: &str, config: Option<&Config>) -> anyhow::Result<ProviderInfo> {
    // A configured provider entry (by id) decides the kind + any base_url override.
    let configured = config.and_then(|config| {
        config
            .providers
            .get(provider)
            .map(|entry| (entry.kind.clone(), entry.base_url.clone()))
    });
    let (kind, base_override) = configured.unwrap_or_else(|| (provider.to_string(), None));

    match kind.as_str() {
        "anthropic" => Ok(ProviderInfo {
            id: provider.to_string(),
            display: "Anthropic".to_string(),
            key_url: ANTHROPIC_KEY_URL.to_string(),
            wire: Wire::Anthropic,
            base_url: base_override.unwrap_or_else(|| ANTHROPIC_DEFAULT_BASE_URL.to_string()),
        }),
        "openai" | "openai-compatible" => Ok(ProviderInfo {
            id: provider.to_string(),
            display: "OpenAI".to_string(),
            key_url: OPENAI_KEY_URL.to_string(),
            wire: Wire::OpenAi,
            base_url: base_override.unwrap_or_else(|| OPENAI_DEFAULT_BASE_URL.to_string()),
        }),
        other => Err(anyhow!(
            "login supports 'anthropic' and 'openai'; '{provider}' is kind '{other}'. \
             Set its API key via its api_key_env environment variable instead."
        )),
    }
}

/// Issue one minimal, documented public request to check the key works. Returns
/// `Ok(true)` on success, `Ok(false)` on an auth rejection, and an error only when
/// the request could not be made (offline). Never logs the key.
async fn validate(info: &ProviderInfo, key: &Secret) -> anyhow::Result<bool> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let url = format!("{}/models", info.base_url.trim_end_matches('/'));
    let request = match info.wire {
        Wire::OpenAi => client.get(&url).bearer_auth(key.expose()),
        Wire::Anthropic => client
            .get(&url)
            .header("x-api-key", key.expose())
            .header("anthropic-version", ANTHROPIC_VERSION),
    };
    let response = request.send().await?;
    let status = response.status();
    // 401/403 is a definitive rejection; any 2xx is acceptance. Other statuses
    // (e.g. 404 on a gateway without a listing) are treated as "could not verify"
    // rather than a hard rejection, so a valid key behind an odd endpoint stores.
    if status.is_success() {
        Ok(true)
    } else if status == reqwest::StatusCode::UNAUTHORIZED
        || status == reqwest::StatusCode::FORBIDDEN
    {
        Ok(false)
    } else {
        Err(anyhow!("unexpected status {status}"))
    }
}

/// Mask a credential for display: keep a short head and tail, hide the middle.
/// Short keys are fully masked.
fn mask(key: &str) -> String {
    let key = key.trim();
    if key.len() <= 8 {
        return "*".repeat(key.len().max(1));
    }
    format!("{}…{}", &key[..4], &key[key.len() - 4..])
}

/// Open `url` in the default browser, best-effort. Any failure is ignored — the
/// URL is always printed as the fallback.
fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", "", url]);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg(url);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(url);
        command
    };
    let _ = command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn load_config() -> Option<Config> {
    let cwd = std::env::current_dir().ok()?;
    localpilot_config::load(&ConfigPaths::standard(&cwd), &CliOverrides::default()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anthropic_info() -> ProviderInfo {
        resolve_provider("anthropic", None).unwrap()
    }

    #[test]
    fn resolves_the_two_supported_providers_and_rejects_others() {
        let anthropic = resolve_provider("anthropic", None).unwrap();
        assert_eq!(anthropic.wire, Wire::Anthropic);
        assert_eq!(anthropic.key_url, ANTHROPIC_KEY_URL);

        let openai = resolve_provider("openai", None).unwrap();
        assert_eq!(openai.wire, Wire::OpenAi);
        assert_eq!(openai.key_url, OPENAI_KEY_URL);

        // An unsupported kind is a clear error, never a panic.
        assert!(resolve_provider("ollama", None).is_err());
    }

    #[test]
    fn a_configured_provider_id_maps_through_its_kind_and_base_url() {
        use localpilot_config::{Config, ProviderConfig};
        let mut config = Config::default();
        config.providers.insert(
            "claude".to_string(),
            ProviderConfig {
                kind: "anthropic".to_string(),
                base_url: Some("https://gateway.example/v1".to_string()),
                ..ProviderConfig::default()
            },
        );
        let info = resolve_provider("claude", Some(&config)).unwrap();
        // The stored account id follows the configured provider id.
        assert_eq!(info.id, "claude");
        assert_eq!(info.wire, Wire::Anthropic);
        assert_eq!(info.base_url, "https://gateway.example/v1");
    }

    #[test]
    fn masking_hides_the_middle_and_fully_masks_short_keys() {
        assert_eq!(mask("sk-abcdefghijkl"), "sk-a…ijkl");
        assert_eq!(mask("short"), "*****");
        // The full key never appears in the masked form.
        assert!(!mask("sk-supersecretvalue").contains("supersecret"));
    }

    #[tokio::test]
    async fn login_stores_a_pasted_key_and_echoes_it_masked() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::with_file(Some(dir.path().join("credentials.json")));
        let info = anthropic_info();
        let mut out = Vec::new();
        login_with(
            &info,
            LoginOptions {
                no_browser: true,
                no_verify: true,
            },
            &Secret::new("sk-ant-paste-test-key"),
            &store,
            &mut out,
        )
        .await
        .unwrap();

        let text = String::from_utf8(out).unwrap();
        // The output confirms storage and shows only a masked form.
        assert!(text.contains("stored anthropic credential"));
        assert!(text.contains("sk-a…-key"));
        assert!(
            !text.contains("sk-ant-paste-test-key"),
            "the full key must never be echoed: {text}"
        );
    }

    #[tokio::test]
    async fn a_provider_rejected_key_is_not_stored_without_the_explicit_override() {
        use localpilot_config::{Config, ProviderConfig};
        // A stub endpoint answering 401 — the definitive rejection leg.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            for stream in listener.incoming().take(2) {
                let Ok(mut stream) = stream else { break };
                let mut buffer = [0_u8; 2048];
                let _ = stream.read(&mut buffer);
                let _ = stream.write_all(
                    b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                );
            }
        });
        let mut config = Config::default();
        config.providers.insert(
            "claude".to_string(),
            ProviderConfig {
                kind: "anthropic".to_string(),
                base_url: Some(format!("http://{addr}")),
                ..ProviderConfig::default()
            },
        );
        let info = resolve_provider("claude", Some(&config)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::with_file(Some(dir.path().join("credentials.json")));

        // Rejected: nothing stored, and the error names the override.
        let result = login_with(
            &info,
            LoginOptions {
                no_browser: true,
                no_verify: false,
            },
            &Secret::new("sk-rejected-key"),
            &store,
            &mut Vec::new(),
        )
        .await;
        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("rejected the key"), "{message}");
        assert!(message.contains("--no-verify"), "{message}");
        assert!(
            store.get("claude").is_none(),
            "a provider-rejected key must not be persisted"
        );

        // The explicit override stores it anyway (offline/gateway escape hatch).
        let mut out = Vec::new();
        login_with(
            &info,
            LoginOptions {
                no_browser: true,
                no_verify: true,
            },
            &Secret::new("sk-rejected-key"),
            &store,
            &mut out,
        )
        .await
        .unwrap();
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("stored claude credential"));
        assert!(store.get("claude").is_some());
    }

    #[tokio::test]
    async fn an_empty_pasted_key_is_rejected_without_storing() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::with_file(Some(dir.path().join("credentials.json")));
        let result = login_with(
            &anthropic_info(),
            LoginOptions {
                no_browser: true,
                no_verify: true,
            },
            &Secret::new("   "),
            &store,
            &mut Vec::new(),
        )
        .await;
        assert!(result.is_err());
    }

    #[test]
    fn logout_removes_a_stored_credential_then_reports_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let store = CredentialStore::with_file(Some(dir.path().join("credentials.json")));
        // A test-only id so a keychain-backed run (if built that way) self-cleans.
        let id = "localpilot-logout-test-provider";
        store.set(id, &Secret::new("sk-logout-test")).unwrap();

        let mut out = Vec::new();
        logout_with(id, &store, &mut out).unwrap();
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("removed the stored localpilot-logout-test-provider credential"));

        // A second logout reports nothing to remove, not an error.
        let mut out = Vec::new();
        logout_with(id, &store, &mut out).unwrap();
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("no stored localpilot-logout-test-provider credential to remove"));
    }
}
