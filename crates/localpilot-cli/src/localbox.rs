//! Read-only detection of a local LocalBox install and a running LocalBox model
//! server.
//!
//! Answers two questions so `localpilot models` (and, later, the interactive
//! model flow) can point a user at a local model without hand-editing config:
//! is a `localbox` binary on `PATH`, and is a LocalBox server already serving —
//! at which endpoint? It never spawns a server, offers to start one, or writes
//! config; that authority lives in later, permission-gated steps. When LocalBox
//! is absent, detection returns [`LocalBoxState::NotInstalled`] after only a
//! cheap `PATH` scan, so every existing flow is left byte-for-byte unchanged.
//!
//! LocalBox exposes no machine-readable status — `localbox status` prints a
//! prose health line on its default ports, carrying no endpoint that could be
//! discovered from it. So the authoritative "is it serving, and where" signal
//! is a read-only probe of LocalBox's documented default proxy endpoint via the
//! existing [`localpilot_llm::discover_models`] — not a parse of that prose,
//! which would couple LocalPilot to a cross-repo wording that can drift.

#[cfg(feature = "tui")]
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
#[cfg(feature = "tui")]
use std::sync::Arc;

use localpilot_llm::discover_models;
#[cfg(feature = "tui")]
use localpilot_llm::ProviderRegistry;
#[cfg(feature = "tui")]
use localpilot_sandbox::{Approver, Profile};
use tokio_util::sync::CancellationToken;

/// LocalBox's documented default no-think proxy endpoint. LocalBox launch and
/// `status` default to proxy `:11435` and backend `:8080` (`--proxy-port` /
/// `--server-port` override them); the proxy serves the OpenAI-compatible `/v1`
/// surface probed here. Mirrored from LocalBox's public defaults — never
/// imported from it.
const DEFAULT_PROXY_BASE_URL: &str = "http://127.0.0.1:11435/v1";

/// Where a detected LocalBox stands. Read-only; no runtime path can panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalBoxState {
    /// No `localbox` binary on `PATH`.
    NotInstalled,
    /// `localbox` is on `PATH`, but no server answered the endpoint probe.
    InstalledNotRunning,
    /// A LocalBox server is serving at `endpoint`; `model` is the first model it
    /// reports, when it reports one.
    Running {
        endpoint: String,
        model: Option<String>,
    },
}

/// Resolve `localbox` on `PATH` without a `which` dependency. Returns the first
/// `PATH` entry holding an executable named `localbox` (with the platform's
/// executable suffix), or `None` when none exists.
pub(crate) fn localbox_on_path() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path_var| localbox_in_paths(&path_var))
}

/// The `PATH`-splitting core of [`localbox_on_path`], taking the `PATH` value so
/// it can be tested without mutating the process environment.
fn localbox_in_paths(path_var: &std::ffi::OsStr) -> Option<PathBuf> {
    std::env::split_paths(path_var).find_map(|dir| {
        let candidate = dir.join(format!("localbox{}", std::env::consts::EXE_SUFFIX));
        candidate.is_file().then_some(candidate)
    })
}

/// Detect LocalBox: presence on `PATH`, then a read-only probe of the documented
/// default proxy endpoint. Never spawns, offers, or writes config.
pub(crate) async fn detect() -> LocalBoxState {
    detect_at(DEFAULT_PROXY_BASE_URL, localbox_on_path()).await
}

/// The endpoint-parameterised core of [`detect`]. `on_path` is the resolved
/// binary (or `None` when LocalBox is not installed); `base_url` is the endpoint
/// to probe. Kept separate so tests drive it against a mock server without a real
/// `localbox` on `PATH`.
async fn detect_at(base_url: &str, on_path: Option<PathBuf>) -> LocalBoxState {
    if on_path.is_none() {
        // Not installed: return immediately without probing, so an absent LocalBox
        // adds no network effect and changes no existing behaviour.
        return LocalBoxState::NotInstalled;
    }
    match discover_models(base_url, None).await {
        Ok(models) => LocalBoxState::Running {
            endpoint: base_url.to_string(),
            model: models.into_iter().next().map(|model| model.id),
        },
        // Installed but the endpoint did not answer as an OpenAI-compatible model
        // listing: not running (or not ready). Never a guess.
        Err(_) => LocalBoxState::InstalledNotRunning,
    }
}

/// What to offer a user who has hit a no-usable-model dead-end, given LocalBox
/// detection. This is the *decision* only — surfacing it (an enriched message at
/// startup, or a permission-gated prompt in the running session) and acting on it
/// belong to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelOffer {
    /// Nothing to offer — the caller shows its existing no-model error unchanged.
    NoOffer,
    /// A LocalBox server is already serving at `endpoint`; point the user at it.
    Running { endpoint: String },
    /// LocalBox is installed but not serving; suggest starting it.
    InstalledNotRunning,
}

/// Decide what to offer at a no-usable-model dead-end. Never overrides a working
/// model: when `usable_model_present`, the answer is always [`ModelOffer::NoOffer`],
/// so the offer only ever appears where the user is genuinely stuck.
pub(crate) fn offer_for(usable_model_present: bool, state: LocalBoxState) -> ModelOffer {
    if usable_model_present {
        return ModelOffer::NoOffer;
    }
    match state {
        LocalBoxState::Running { endpoint, .. } => ModelOffer::Running { endpoint },
        LocalBoxState::InstalledNotRunning => ModelOffer::InstalledNotRunning,
        LocalBoxState::NotInstalled => ModelOffer::NoOffer,
    }
}

/// The one-line, actionable pointer for an offer, or `None` when there is
/// nothing to offer. Single source of the offer wording so the startup error and
/// the in-session `/model` notice stay identical.
pub(crate) fn offer_message(offer: &ModelOffer) -> Option<String> {
    match offer {
        ModelOffer::NoOffer => None,
        ModelOffer::Running { endpoint } => Some(format!(
            "a LocalBox server is serving at {endpoint} — add it under [providers.local] in .localpilot.toml to use it"
        )),
        ModelOffer::InstalledNotRunning => Some(
            "LocalBox is installed — run `localbox serve <model>` to start a local model, then retry"
                .to_string(),
        ),
    }
}

/// Merge a `[providers.local]` block for a detected LocalBox proxy endpoint into
/// existing `.localpilot.toml` text, preserving every other table, provider,
/// key, and comment. Unlike LocalBox's own emitter — which owns and
/// wholesale-replaces the `providers` table — this **upserts only
/// `[providers.local]`**, so a user's other `[providers.*]`, `[mcp.servers.*]`,
/// and comments survive. `[provider] default` is pointed at `local` so the
/// adopted server is used. The `kind`/`api_key_env` mirror LocalBox's
/// proxied-route contract (the no-think proxy speaks the Anthropic wire); they
/// are mirrored from LocalBox's public contract, never imported.
///
/// # Errors
/// Returns an error when `existing` is not valid TOML — the caller must fail
/// rather than overwrite content it could not safely merge.
pub(crate) fn merge_local_provider(
    existing: &str,
    endpoint: &str,
    model: Option<&str>,
) -> anyhow::Result<String> {
    use toml_edit::{value, DocumentMut, Item, Table};

    let mut doc: DocumentMut = existing.parse()?;

    // `[provider] default = "local"`. Insert as a real table (not an inline
    // `provider = { .. }`) so a fresh file reads with section headers and a user
    // can later hand-add sibling sections — extending an inline table with a
    // `[..]` section is a TOML parse error.
    let provider = doc.entry("provider").or_insert(Item::Table(Table::new()));
    provider["default"] = value("local");

    // `[providers]` holds only sub-tables — mark it implicit so only
    // `[providers.local]` (and any existing siblings) render their own header.
    let providers = doc.entry("providers").or_insert(Item::Table(Table::new()));
    let providers = providers
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("`providers` in .localpilot.toml is not a table"))?;
    providers.set_implicit(true);

    // Upsert only `[providers.local]`, preserving any other providers.
    let local = providers
        .entry("local")
        .or_insert(Item::Table(Table::new()));
    let local = local
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("`providers.local` in .localpilot.toml is not a table"))?;
    local["kind"] = value("anthropic");
    local["base_url"] = value(endpoint);
    local["api_key_env"] = value("ANTHROPIC_AUTH_TOKEN");
    if let Some(model) = model {
        local["model"] = value(model);
    }
    Ok(doc.to_string())
}

/// Adopt a running LocalBox server into the project's `.localpilot.toml` so
/// LocalPilot uses it. Detects a running server, gates the config write through
/// the permission engine (reusing the `models`-style Allow / confirm / report
/// flow), then merges `[providers.local]`. Writes nothing unless a running
/// server is found and the write is approved.
///
/// # Errors
/// Fails when no running LocalBox is found, the write is declined or needs
/// approval non-interactively, or the config file cannot be read/written.
pub(crate) async fn run_adopt(
    serve: Option<String>,
    assume_yes: bool,
    stdin_is_tty: bool,
) -> anyhow::Result<()> {
    use localpilot_config::{CliOverrides, ConfigPaths};
    use localpilot_sandbox::{
        CommandClass, Decision, Effect, Interactivity, PermissionEngine, PermissionRequest,
    };

    let cwd = std::env::current_dir()?;
    let config = localpilot_config::load(&ConfigPaths::standard(&cwd), &CliOverrides::default())?;
    let engine = PermissionEngine::new(crate::models_cmd::profile(&config), Vec::new());
    let interactivity = if stdin_is_tty && !assume_yes {
        Interactivity::Interactive
    } else {
        Interactivity::NonInteractive
    };
    // One consent step for every gated side effect (start the server, write config).
    let consent = |request: &PermissionRequest, question: &str| -> anyhow::Result<bool> {
        if assume_yes {
            return Ok(true);
        }
        Ok(match engine.decide(request) {
            Decision::Allow => true,
            Decision::Ask if stdin_is_tty => crate::models_cmd::confirm(question)?,
            Decision::Ask => {
                anyhow::bail!(
                    "this needs approval — re-run with --yes to proceed non-interactively"
                )
            }
            Decision::Deny => false,
        })
    };

    // Resolve a running server, starting one first when asked and none is up.
    let mut state = detect().await;
    if !matches!(state, LocalBoxState::Running { .. }) {
        match (&state, serve.as_deref()) {
            (LocalBoxState::NotInstalled, _) => {
                anyhow::bail!("LocalBox is not installed (no `localbox` on PATH)")
            }
            (LocalBoxState::InstalledNotRunning, None) => anyhow::bail!(
                "no running LocalBox server found — pass --serve <model> to start one, or run `localbox serve <model>` first (see `localbox info`)"
            ),
            (LocalBoxState::InstalledNotRunning, Some(model)) => {
                // Starting a server loads a model and binds a port — a real side
                // effect, gated like any other.
                let permission = PermissionRequest {
                    tool: "localbox serve".to_string(),
                    effect: Effect::RunCommand(CommandClass::ExternalWrite),
                    interactivity,
                    trusted: true,
                    detail: format!("localbox serve {model}"),
                };
                if !consent(
                    &permission,
                    &format!("start LocalBox serving {model} (loads a model, can take minutes)?"),
                )? {
                    anyhow::bail!("declined — LocalBox not started");
                }
                let _ = start_localbox_serve(model, ServeStdio::Inherit, None).await?;
                state = detect().await;
            }
            (LocalBoxState::Running { .. }, _) => {}
        }
    }

    let (endpoint, model) = match state {
        LocalBoxState::Running { endpoint, model } => (endpoint, model),
        _ => anyhow::bail!(
            "LocalBox did not come up at {DEFAULT_PROXY_BASE_URL} after starting — check `localbox status`"
        ),
    };

    // Writing `.localpilot.toml` is a workspace file write; gate it too.
    let path = localpilot_config::project_config_path(&cwd);
    let request = PermissionRequest {
        tool: "localbox adopt".to_string(),
        effect: Effect::WritePath {
            inside_workspace: true,
            overwrite: path.exists(),
            secret_like: false,
        },
        interactivity,
        trusted: true,
        detail: path.display().to_string(),
    };
    if !consent(
        &request,
        &format!(
            "add [providers.local] for {endpoint} to {}?",
            path.display()
        ),
    )? {
        anyhow::bail!("declined — no config written");
    }
    write_local_provider(&path, &endpoint, model.as_deref())?;
    println!(
        "adopted LocalBox — wrote [providers.local] for {endpoint} to {}",
        path.display()
    );
    Ok(())
}

/// Child stdio policy for `localbox serve`: the standalone CLI keeps the visible
/// progress stream, while terminal hosts isolate all streams from their raw-mode
/// alternate screen.
#[derive(Clone, Copy)]
enum ServeStdio {
    Inherit,
    #[cfg(feature = "tui")]
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServeWait {
    Complete,
    Cancelled,
}

/// Run LocalBox's blocking launcher without blocking Tokio. When `cancel` fires,
/// LocalPilot drops only its wait handle; Tokio documents that the child keeps
/// running by default, which preserves ADR-0130's ownership boundary. LocalBox
/// remains the sole owner of the detached model server and its teardown.
async fn start_localbox_serve(
    model: &str,
    stdio: ServeStdio,
    cancel: Option<&CancellationToken>,
) -> anyhow::Result<ServeWait> {
    let program = localbox_on_path()
        .ok_or_else(|| anyhow::anyhow!("LocalBox is not installed (no `localbox` on PATH)"))?;
    let mut command = tokio::process::Command::new(program);
    command.arg("serve").arg(model).kill_on_drop(false);
    match stdio {
        ServeStdio::Inherit => {
            println!("starting LocalBox serving {model} (loading a model can take a few minutes)…");
            command
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
        }
        #[cfg(feature = "tui")]
        ServeStdio::Null => {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
        }
    }
    let mut child = command
        .spawn()
        .map_err(|error| anyhow::anyhow!("could not run `localbox serve {model}`: {error}"))?;
    let status = wait_or_cancel(child.wait(), cancel).await;
    let Some(status) = status else {
        // Deliberate detach: dropping Tokio's `Child` with kill_on_drop(false)
        // leaves `localbox serve` running. It may still finish launching the
        // LocalBox-owned server after the TUI returns to idle.
        return Ok(ServeWait::Cancelled);
    };
    let status = status?;
    if !status.success() {
        anyhow::bail!("`localbox serve {model}` exited with {status}");
    }
    Ok(ServeWait::Complete)
}

async fn wait_or_cancel<F, T>(wait: F, cancel: Option<&CancellationToken>) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    match cancel {
        Some(cancel) => {
            tokio::select! {
                value = wait => Some(value),
                _ = cancel.cancelled() => None,
            }
        }
        None => Some(wait.await),
    }
}

/// How a terminal host resolves an `Ask` decision. The legacy inline host keeps
/// its standard approval dialog; the full-screen host preserves ADR-0130's
/// explicit-command consent while still honoring hard `Deny` decisions.
#[cfg(feature = "tui")]
pub(crate) enum TerminalConsent<'a> {
    Prompt(&'a dyn Approver),
    ExplicitCommand,
}

/// A successful terminal adoption, including the freshly rebuilt provider
/// registry needed to activate `local` in the current idle session.
#[cfg(feature = "tui")]
pub(crate) struct AdoptedLocalBox {
    pub(crate) endpoint: String,
    pub(crate) config: localpilot_config::Config,
    pub(crate) registry: Arc<ProviderRegistry>,
}

/// Observable outcome of the in-terminal workflow.
#[cfg(feature = "tui")]
pub(crate) enum TerminalAdoptOutcome {
    Adopted(Box<AdoptedLocalBox>),
    Declined(&'static str),
    Cancelled,
}

/// Detect, optionally launch, permission-gate, adopt, reload, and rebuild the
/// provider registry for a terminal host. The caller owns UI projection and the
/// final idle runtime switch; config is durable before this returns `Adopted`.
#[cfg(feature = "tui")]
pub(crate) async fn run_terminal_adopt(
    cwd: &Path,
    serve: Option<&str>,
    profile: Profile,
    trusted: bool,
    consent: TerminalConsent<'_>,
    cancel: &CancellationToken,
) -> anyhow::Result<TerminalAdoptOutcome> {
    let request = TerminalAdoptRequest {
        cwd,
        serve,
        profile,
        trusted,
        consent,
        cancel,
    };
    run_terminal_adopt_with(request, detect, |model, operation_cancel| async move {
        start_localbox_serve(&model, ServeStdio::Null, Some(&operation_cancel)).await
    })
    .await
}

#[cfg(feature = "tui")]
struct TerminalAdoptRequest<'a> {
    cwd: &'a Path,
    serve: Option<&'a str>,
    profile: Profile,
    trusted: bool,
    consent: TerminalConsent<'a>,
    cancel: &'a CancellationToken,
}

#[cfg(feature = "tui")]
async fn run_terminal_adopt_with<D, DetectFuture, S, ServeFuture>(
    request: TerminalAdoptRequest<'_>,
    mut detect_state: D,
    mut start_serve: S,
) -> anyhow::Result<TerminalAdoptOutcome>
where
    D: FnMut() -> DetectFuture,
    DetectFuture: std::future::Future<Output = LocalBoxState>,
    S: FnMut(String, CancellationToken) -> ServeFuture,
    ServeFuture: std::future::Future<Output = anyhow::Result<ServeWait>>,
{
    use localpilot_sandbox::{
        CommandClass, Effect, Interactivity, PermissionEngine, PermissionRequest,
    };

    let engine = PermissionEngine::new(request.profile, Vec::new());
    let mut state = detect_state().await;
    if !matches!(state, LocalBoxState::Running { .. }) {
        match (&state, request.serve) {
            (LocalBoxState::NotInstalled, _) => {
                anyhow::bail!("LocalBox is not installed (no `localbox` on PATH)")
            }
            (LocalBoxState::InstalledNotRunning, None) => anyhow::bail!(
                "no running LocalBox server found — use `/localbox adopt --serve <model>` or run `localbox serve <model>` first"
            ),
            (LocalBoxState::InstalledNotRunning, Some(model)) => {
                let permission = PermissionRequest {
                    tool: "localbox serve".to_string(),
                    effect: Effect::RunCommand(CommandClass::ExternalWrite),
                    interactivity: Interactivity::Interactive,
                    trusted: request.trusted,
                    detail: format!("localbox serve {model}"),
                };
                if !terminal_effect_allowed(&engine, &permission, &request.consent).await {
                    return Ok(TerminalAdoptOutcome::Declined("LocalBox launch"));
                }
                if matches!(
                    start_serve(model.to_string(), request.cancel.clone()).await?,
                    ServeWait::Cancelled
                ) {
                    return Ok(TerminalAdoptOutcome::Cancelled);
                }
                state = detect_state().await;
            }
            (LocalBoxState::Running { .. }, _) => {}
        }
    }

    let (endpoint, model) = match state {
        LocalBoxState::Running { endpoint, model } => (endpoint, model),
        LocalBoxState::InstalledNotRunning | LocalBoxState::NotInstalled => anyhow::bail!(
            "LocalBox did not come up at {DEFAULT_PROXY_BASE_URL} after starting — check `localbox status`"
        ),
    };
    let path = localpilot_config::project_config_path(request.cwd);
    let permission = PermissionRequest {
        tool: "localbox adopt".to_string(),
        effect: Effect::WritePath {
            inside_workspace: true,
            overwrite: path.exists(),
            secret_like: false,
        },
        interactivity: Interactivity::Interactive,
        trusted: request.trusted,
        detail: path.display().to_string(),
    };
    if !terminal_effect_allowed(&engine, &permission, &request.consent).await {
        return Ok(TerminalAdoptOutcome::Declined("LocalBox adoption"));
    }
    write_local_provider(&path, &endpoint, model.as_deref())?;

    let config = localpilot_config::load(
        &localpilot_config::ConfigPaths::standard(request.cwd),
        &localpilot_config::CliOverrides::default(),
    )?;
    let registry = Arc::new(ProviderRegistry::from_config(&config)?);
    Ok(TerminalAdoptOutcome::Adopted(Box::new(AdoptedLocalBox {
        endpoint,
        config,
        registry,
    })))
}

#[cfg(feature = "tui")]
async fn terminal_effect_allowed(
    engine: &localpilot_sandbox::PermissionEngine,
    request: &localpilot_sandbox::PermissionRequest,
    consent: &TerminalConsent<'_>,
) -> bool {
    match engine.decide(request) {
        localpilot_sandbox::Decision::Allow => true,
        localpilot_sandbox::Decision::Ask => match consent {
            TerminalConsent::Prompt(approver) => approver.approve(request).await,
            TerminalConsent::ExplicitCommand => true,
        },
        localpilot_sandbox::Decision::Deny => false,
    }
}

/// Merge a `[providers.local]` block for `endpoint`/`model` into the config file
/// at `path` (creating it if absent), preserving other content. The **surface-
/// agnostic write half of adopt**: the CLI (`run_adopt`) and the in-TUI `/model`
/// adopt both reach here only after consent — CLI confirm or an in-session
/// permission approval — so gating is the caller's responsibility, never this
/// function's. Reached only on approval, so a denied grant writes nothing.
///
/// # Errors
/// Propagates a malformed existing file (never overwrites unparseable content)
/// or an I/O error writing the file.
pub(crate) fn write_local_provider(
    path: &std::path::Path,
    endpoint: &str,
    model: Option<&str>,
) -> anyhow::Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let merged = merge_local_provider(&existing, endpoint, model)?;
    std::fs::write(path, merged)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn adopt_merge_upserts_only_providers_local_and_preserves_siblings() {
        let existing = "\
[provider]
default = \"openai\"

[providers.openai]
kind = \"openai\"
api_key_env = \"OPENAI_API_KEY\"

# a hand-added MCP server
[mcp.servers.playwright]
command = \"npx\"
";
        let merged =
            merge_local_provider(existing, "http://127.0.0.1:11435/v1", Some("qwen-coder"))
                .unwrap();
        let doc: toml_edit::DocumentMut = merged.parse().unwrap();
        // The sibling provider and the MCP table survive untouched.
        assert_eq!(doc["providers"]["openai"]["kind"].as_str(), Some("openai"));
        assert_eq!(
            doc["mcp"]["servers"]["playwright"]["command"].as_str(),
            Some("npx")
        );
        assert!(merged.contains("# a hand-added MCP server"));
        // The LocalBox proxy contract is written, and default points at local.
        assert_eq!(
            doc["providers"]["local"]["kind"].as_str(),
            Some("anthropic")
        );
        assert_eq!(
            doc["providers"]["local"]["base_url"].as_str(),
            Some("http://127.0.0.1:11435/v1")
        );
        assert_eq!(
            doc["providers"]["local"]["api_key_env"].as_str(),
            Some("ANTHROPIC_AUTH_TOKEN")
        );
        assert_eq!(
            doc["providers"]["local"]["model"].as_str(),
            Some("qwen-coder")
        );
        assert_eq!(doc["provider"]["default"].as_str(), Some("local"));
    }

    #[test]
    fn adopt_merge_into_empty_config_creates_local_provider_without_a_model() {
        let merged = merge_local_provider("", "http://127.0.0.1:11435/v1", None).unwrap();
        // A fresh file uses section headers, not inline tables — so a user can
        // later hand-add a sibling `[providers.<id>]` without a TOML parse error.
        assert!(
            merged.contains("[providers.local]"),
            "fresh config should use section headers, got: {merged}"
        );
        let doc: toml_edit::DocumentMut = merged.parse().unwrap();
        assert_eq!(
            doc["providers"]["local"]["kind"].as_str(),
            Some("anthropic")
        );
        assert_eq!(doc["provider"]["default"].as_str(), Some("local"));
        assert!(doc["providers"]["local"].get("model").is_none());
    }

    #[test]
    fn adopt_merge_rejects_a_malformed_existing_file() {
        assert!(merge_local_provider("not [ valid", "http://x/v1", None).is_err());
    }

    #[test]
    fn write_local_provider_merges_into_the_project_config_preserving_siblings() {
        // The shared write half: reached only after consent, it writes the
        // merged config to disk and preserves the user's other providers.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".localpilot.toml");
        std::fs::write(&path, "[providers.openai]\nkind = \"openai\"\n").unwrap();
        write_local_provider(&path, "http://127.0.0.1:11435/v1", Some("m")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let doc: toml_edit::DocumentMut = text.parse().unwrap();
        assert_eq!(
            doc["providers"]["local"]["kind"].as_str(),
            Some("anthropic")
        );
        assert_eq!(doc["providers"]["openai"]["kind"].as_str(), Some("openai"));
        assert_eq!(doc["provider"]["default"].as_str(), Some("local"));
    }

    #[test]
    fn write_local_provider_creates_the_file_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".localpilot.toml");
        write_local_provider(&path, "http://127.0.0.1:11435/v1", None).unwrap();
        assert!(path.is_file());
        let doc: toml_edit::DocumentMut = std::fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(
            doc["providers"]["local"]["base_url"].as_str(),
            Some("http://127.0.0.1:11435/v1")
        );
    }

    #[test]
    fn offer_message_is_actionable_and_none_when_nothing_to_offer() {
        assert_eq!(offer_message(&ModelOffer::NoOffer), None);
        assert!(offer_message(&ModelOffer::Running {
            endpoint: "http://127.0.0.1:11435/v1".to_string(),
        })
        .unwrap()
        .contains("[providers.local]"));
        assert!(offer_message(&ModelOffer::InstalledNotRunning)
            .unwrap()
            .contains("localbox serve"));
    }

    #[test]
    fn offers_only_when_no_usable_model_and_localbox_present() {
        assert_eq!(
            offer_for(
                false,
                LocalBoxState::Running {
                    endpoint: "http://127.0.0.1:11435/v1".to_string(),
                    model: None,
                },
            ),
            ModelOffer::Running {
                endpoint: "http://127.0.0.1:11435/v1".to_string(),
            }
        );
        assert_eq!(
            offer_for(false, LocalBoxState::InstalledNotRunning),
            ModelOffer::InstalledNotRunning
        );
        // Not installed → no offer (the caller keeps its exact legacy error).
        assert_eq!(
            offer_for(false, LocalBoxState::NotInstalled),
            ModelOffer::NoOffer
        );
    }

    #[test]
    fn no_offer_when_a_model_is_reachable() {
        // A usable model already exists — never offer, even if LocalBox is running.
        assert_eq!(
            offer_for(
                true,
                LocalBoxState::Running {
                    endpoint: "http://127.0.0.1:11435/v1".to_string(),
                    model: Some("qwen-coder".to_string()),
                },
            ),
            ModelOffer::NoOffer
        );
    }

    #[test]
    fn detects_localbox_absent_when_not_on_path() {
        // A PATH with no `localbox` binary resolves to None.
        let empty = tempfile::tempdir().unwrap();
        let path_var = std::env::join_paths([empty.path()]).unwrap();
        assert_eq!(localbox_in_paths(&path_var), None);
    }

    #[test]
    fn finds_localbox_on_path_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir
            .path()
            .join(format!("localbox{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&bin, b"").unwrap();
        let path_var = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(localbox_in_paths(&path_var), Some(bin));
    }

    #[tokio::test]
    async fn running_server_is_detected_via_default_endpoint_probe() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [ { "id": "qwen-coder", "object": "model" } ]
            })))
            .mount(&server)
            .await;
        let endpoint = format!("{}/v1", server.uri());
        let state = detect_at(&endpoint, Some(PathBuf::from("localbox"))).await;
        assert_eq!(
            state,
            LocalBoxState::Running {
                endpoint,
                model: Some("qwen-coder".to_string()),
            }
        );
    }

    #[tokio::test]
    async fn cancelled_child_wait_returns_without_waiting_for_the_child_future() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            wait_or_cancel(std::future::pending::<()>(), Some(&cancel)),
        )
        .await
        .unwrap();
        assert!(outcome.is_none());
    }

    #[cfg(feature = "tui")]
    #[tokio::test]
    async fn terminal_consent_prompts_on_ask_while_explicit_command_is_consent() {
        use localpilot_sandbox::{
            CommandClass, Effect, Interactivity, PermissionEngine, PermissionRequest, Profile,
            ScriptedApprover,
        };

        let engine = PermissionEngine::new(Profile::Default, Vec::new());
        let request = PermissionRequest {
            tool: "localbox serve".to_string(),
            effect: Effect::RunCommand(CommandClass::ExternalWrite),
            interactivity: Interactivity::Interactive,
            trusted: true,
            detail: "localbox serve model.gguf".to_string(),
        };
        let denied = ScriptedApprover::new(vec![false]);
        assert!(
            !terminal_effect_allowed(&engine, &request, &TerminalConsent::Prompt(&denied)).await
        );
        let approved = ScriptedApprover::new(vec![true]);
        assert!(
            terminal_effect_allowed(&engine, &request, &TerminalConsent::Prompt(&approved)).await
        );
        assert!(
            terminal_effect_allowed(&engine, &request, &TerminalConsent::ExplicitCommand).await
        );
    }

    #[cfg(feature = "tui")]
    #[tokio::test]
    async fn terminal_adopt_of_a_running_server_writes_and_builds_the_live_registry() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let outcome = run_terminal_adopt_with(
            TerminalAdoptRequest {
                cwd: dir.path(),
                serve: None,
                profile: Profile::Unrestricted,
                trusted: true,
                consent: TerminalConsent::ExplicitCommand,
                cancel: &cancel,
            },
            || {
                std::future::ready(LocalBoxState::Running {
                    endpoint: DEFAULT_PROXY_BASE_URL.to_string(),
                    model: Some("bonsai.gguf".to_string()),
                })
            },
            |_model, _cancel| std::future::ready(Err(anyhow::anyhow!("unexpected launch"))),
        )
        .await
        .unwrap();

        let TerminalAdoptOutcome::Adopted(adopted) = outcome else {
            panic!("expected an adopted provider");
        };
        assert_eq!(adopted.endpoint, DEFAULT_PROXY_BASE_URL);
        assert!(adopted.registry.get("local").is_some());
        assert_eq!(
            adopted
                .config
                .providers
                .get("local")
                .and_then(|provider| provider.model.as_deref()),
            Some("bonsai.gguf")
        );
        assert!(localpilot_config::project_config_path(dir.path()).is_file());
    }

    #[cfg(feature = "tui")]
    #[tokio::test]
    async fn terminal_serve_uses_the_literal_model_then_adopts_when_ready() {
        use std::cell::RefCell;
        use std::collections::VecDeque;

        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let states = RefCell::new(VecDeque::from([
            LocalBoxState::InstalledNotRunning,
            LocalBoxState::Running {
                endpoint: DEFAULT_PROXY_BASE_URL.to_string(),
                model: Some("Bonsai 27B.gguf".to_string()),
            },
        ]));
        let started = RefCell::new(Vec::new());
        let outcome = run_terminal_adopt_with(
            TerminalAdoptRequest {
                cwd: dir.path(),
                serve: Some("Bonsai 27B.gguf"),
                profile: Profile::Unrestricted,
                trusted: true,
                consent: TerminalConsent::ExplicitCommand,
                cancel: &cancel,
            },
            || {
                std::future::ready(
                    states
                        .borrow_mut()
                        .pop_front()
                        .unwrap_or(LocalBoxState::InstalledNotRunning),
                )
            },
            |model, _cancel| {
                started.borrow_mut().push(model);
                std::future::ready(Ok(ServeWait::Complete))
            },
        )
        .await
        .unwrap();

        assert!(matches!(outcome, TerminalAdoptOutcome::Adopted(_)));
        assert_eq!(started.into_inner(), vec!["Bonsai 27B.gguf"]);
    }

    #[cfg(feature = "tui")]
    #[tokio::test]
    async fn terminal_serve_cancel_and_failure_write_nothing() {
        for failure in [None, Some("launcher failed")] {
            let dir = tempfile::tempdir().unwrap();
            let cancel = CancellationToken::new();
            let outcome = run_terminal_adopt_with(
                TerminalAdoptRequest {
                    cwd: dir.path(),
                    serve: Some("model.gguf"),
                    profile: Profile::Unrestricted,
                    trusted: true,
                    consent: TerminalConsent::ExplicitCommand,
                    cancel: &cancel,
                },
                || std::future::ready(LocalBoxState::InstalledNotRunning),
                |_model, _cancel| {
                    std::future::ready(match failure {
                        None => Ok(ServeWait::Cancelled),
                        Some(message) => Err(anyhow::anyhow!(message)),
                    })
                },
            )
            .await;
            match failure {
                None => assert!(matches!(outcome, Ok(TerminalAdoptOutcome::Cancelled))),
                Some(message) => {
                    assert!(outcome.is_err_and(|error| error.to_string().contains(message)))
                }
            }
            assert!(!localpilot_config::project_config_path(dir.path()).exists());
        }
    }

    #[cfg(feature = "tui")]
    #[tokio::test]
    async fn terminal_decline_absence_and_bad_config_never_overwrite() {
        use localpilot_sandbox::ScriptedApprover;

        let cancel = CancellationToken::new();
        let declined_dir = tempfile::tempdir().unwrap();
        let declined_path = localpilot_config::project_config_path(declined_dir.path());
        std::fs::write(&declined_path, "marker = true\n").unwrap();
        let approver = ScriptedApprover::new(vec![false]);
        let declined = run_terminal_adopt_with(
            TerminalAdoptRequest {
                cwd: declined_dir.path(),
                serve: None,
                profile: Profile::Default,
                trusted: false,
                consent: TerminalConsent::Prompt(&approver),
                cancel: &cancel,
            },
            || {
                std::future::ready(LocalBoxState::Running {
                    endpoint: DEFAULT_PROXY_BASE_URL.to_string(),
                    model: None,
                })
            },
            |_model, _cancel| std::future::ready(Ok(ServeWait::Complete)),
        )
        .await
        .unwrap();
        assert!(matches!(
            declined,
            TerminalAdoptOutcome::Declined("LocalBox adoption")
        ));
        assert_eq!(
            std::fs::read_to_string(declined_path).unwrap(),
            "marker = true\n"
        );

        let absent_dir = tempfile::tempdir().unwrap();
        let absent = run_terminal_adopt_with(
            TerminalAdoptRequest {
                cwd: absent_dir.path(),
                serve: Some("model.gguf"),
                profile: Profile::Unrestricted,
                trusted: true,
                consent: TerminalConsent::ExplicitCommand,
                cancel: &cancel,
            },
            || std::future::ready(LocalBoxState::NotInstalled),
            |_model, _cancel| std::future::ready(Ok(ServeWait::Complete)),
        )
        .await;
        assert!(absent.is_err_and(|error| error.to_string().contains("not installed")));

        let malformed_dir = tempfile::tempdir().unwrap();
        let path = localpilot_config::project_config_path(malformed_dir.path());
        std::fs::write(&path, "not [ valid").unwrap();
        let malformed = run_terminal_adopt_with(
            TerminalAdoptRequest {
                cwd: malformed_dir.path(),
                serve: None,
                profile: Profile::Unrestricted,
                trusted: true,
                consent: TerminalConsent::ExplicitCommand,
                cancel: &cancel,
            },
            || {
                std::future::ready(LocalBoxState::Running {
                    endpoint: DEFAULT_PROXY_BASE_URL.to_string(),
                    model: None,
                })
            },
            |_model, _cancel| std::future::ready(Ok(ServeWait::Complete)),
        )
        .await;
        assert!(malformed.is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "not [ valid");
    }

    #[tokio::test]
    async fn unreachable_endpoint_degrades_to_installed_not_running() {
        // Installed (path present) but nothing serving on the endpoint → not
        // running, reported as such rather than guessed.
        let state = detect_at("http://127.0.0.1:1/v1", Some(PathBuf::from("localbox"))).await;
        assert_eq!(state, LocalBoxState::InstalledNotRunning);
    }

    #[tokio::test]
    async fn absent_localbox_never_probes_and_is_not_installed() {
        // With no binary on PATH, detection returns NotInstalled without probing:
        // the pre-existing behaviour is untouched (the seam is inert).
        let state = detect_at("http://127.0.0.1:1/v1", None).await;
        assert_eq!(state, LocalBoxState::NotInstalled);
    }
}
