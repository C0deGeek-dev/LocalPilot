//! Detection, permission-gated launch, and provider adoption for a local
//! LocalBox model server.
//!
//! Detection answers whether a `localbox` binary is on `PATH` and whether a
//! server is already serving. The CLI and terminal-chat workflows can then
//! launch a requested model, wait for readiness, and upsert the local provider
//! after their host-specific permission gates. When LocalBox is absent,
//! detection returns [`LocalBoxState::NotInstalled`] after only a cheap `PATH`
//! scan, so unrelated provider flows are unchanged.
//!
//! LocalBox's versioned `models --json` contract is authoritative for models
//! available to launch and their run-profile state. Live readiness/current
//! identity still comes from a read-only probe of LocalBox's documented default
//! proxy endpoint via [`localpilot_llm::discover_models`]; `localbox status`
//! remains human prose and is never parsed.

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

use serde::Deserialize;

/// LocalBox's documented default no-think proxy endpoint. LocalBox launch and
/// `status` default to proxy `:11435` and backend `:8080` (`--proxy-port` /
/// `--server-port` override them); the proxy serves the OpenAI-compatible `/v1`
/// surface probed here. Mirrored from LocalBox's public defaults — never
/// imported from it.
const DEFAULT_PROXY_BASE_URL: &str = "http://127.0.0.1:11435/v1";
const MODELS_CATALOG_SCHEMA: u32 = 1;
const MAX_CATALOG_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
struct LocalBoxModelsCatalog {
    schema: u32,
    models: Vec<LocalBoxModelEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct LocalBoxModelEntry {
    name: String,
    #[serde(default)]
    aliases: Vec<String>,
    display_name: Option<String>,
    repository: String,
    default_quant: Option<String>,
    required_mode: Option<String>,
    run_profile: LocalBoxRunProfile,
    #[serde(default)]
    active: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
struct LocalBoxRunProfile {
    source: String,
    source_path: String,
    reason: Option<String>,
    warning: Option<String>,
    quant: Option<String>,
    context: Option<String>,
    mode: Option<String>,
}

fn safe_catalog_field(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control())
        .take(max_chars)
        .collect()
}

async fn read_models_catalog() -> anyhow::Result<LocalBoxModelsCatalog> {
    let program = localbox_on_path()
        .ok_or_else(|| anyhow::anyhow!("LocalBox is not installed (no `localbox` on PATH)"))?;
    let output = tokio::process::Command::new(program)
        .args(["models", "--json"])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|error| anyhow::anyhow!("could not run `localbox models --json`: {error}"))?;
    if output.stdout.len() > MAX_CATALOG_BYTES || output.stderr.len() > MAX_CATALOG_BYTES {
        anyhow::bail!("LocalBox model catalog exceeded the 1 MiB safety limit");
    }
    if !output.status.success() {
        let detail = safe_catalog_field(&String::from_utf8_lossy(&output.stderr), 500);
        anyhow::bail!(
            "this LocalBox does not provide the model-catalog contract; update LocalBox and retry (you can inspect the older install with `localbox info`){suffix}",
            suffix = if detail.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", detail.trim())
            }
        );
    }
    parse_models_catalog(&output.stdout)
}

fn parse_models_catalog(bytes: &[u8]) -> anyhow::Result<LocalBoxModelsCatalog> {
    let catalog: LocalBoxModelsCatalog = serde_json::from_slice(bytes)
        .map_err(|error| anyhow::anyhow!("LocalBox returned an invalid model catalog: {error}"))?;
    if catalog.schema != MODELS_CATALOG_SCHEMA {
        anyhow::bail!(
            "LocalBox model-catalog schema {} is not supported (expected {}); update LocalPilot and LocalBox together",
            catalog.schema,
            MODELS_CATALOG_SCHEMA
        );
    }
    Ok(catalog)
}

fn catalog_entry<'a>(
    catalog: &'a LocalBoxModelsCatalog,
    name: &str,
) -> Option<&'a LocalBoxModelEntry> {
    catalog
        .models
        .iter()
        .find(|model| model.name == name || model.aliases.iter().any(|alias| alias == name))
}

/// Read and render the LocalBox-owned launch catalog. No server is started and
/// no project file is written.
pub(crate) async fn run_models() -> anyhow::Result<String> {
    let catalog = read_models_catalog().await?;
    let active_model = match detect().await {
        LocalBoxState::Running { model, .. } => model,
        LocalBoxState::NotInstalled | LocalBoxState::InstalledNotRunning => None,
    };
    Ok(render_models_catalog(&catalog, active_model.as_deref()))
}

fn render_models_catalog(catalog: &LocalBoxModelsCatalog, active_model: Option<&str>) -> String {
    let mut out = String::from("LocalBox models\n");
    for model in &catalog.models {
        let name = safe_catalog_field(&model.name, 100);
        let aliases = model
            .aliases
            .iter()
            .map(|alias| safe_catalog_field(alias, 100))
            .collect::<Vec<_>>();
        let alias_text = if aliases.is_empty() {
            String::new()
        } else {
            format!(" (aliases: {})", aliases.join(", "))
        };
        let display = safe_catalog_field(
            model.display_name.as_deref().unwrap_or(&model.repository),
            160,
        );
        let repository = safe_catalog_field(&model.repository, 160);
        let catalog_quant = safe_catalog_field(
            model.default_quant.as_deref().unwrap_or("catalog default"),
            100,
        );
        let profile = if model.run_profile.source == "tuned" {
            format!(
                "tuned {} / {} / {}",
                safe_catalog_field(
                    model
                        .run_profile
                        .quant
                        .as_deref()
                        .unwrap_or("default quant"),
                    80
                ),
                safe_catalog_field(
                    model
                        .run_profile
                        .context
                        .as_deref()
                        .unwrap_or("default context"),
                    80
                ),
                safe_catalog_field(
                    model.run_profile.mode.as_deref().unwrap_or("default mode"),
                    80
                )
            )
        } else {
            format!(
                "defaults ({})",
                safe_catalog_field(
                    model.run_profile.reason.as_deref().unwrap_or("not tuned"),
                    80
                )
            )
        };
        let active = active_model.is_some_and(|active| {
            active == model.name || model.aliases.iter().any(|alias| alias == active)
        }) || model.active == Some(true);
        let mode = model
            .required_mode
            .as_deref()
            .map(|mode| format!(" · mode {}", safe_catalog_field(mode, 40)))
            .unwrap_or_default();
        out.push_str(&format!(
            "{name}{alias_text}{} — {display} · {repository} · quant {catalog_quant}{mode} · {profile}\n",
            if active { " [active]" } else { "" }
        ));
    }
    out.push_str("\nStart and switch: /localbox serve <name>\n");
    if out.len() > 64 * 1024 {
        out.truncate(64 * 1024);
        out.push_str("\n… model catalog truncated …\n");
    }
    out
}

async fn preflight_serve(name: &str) -> anyhow::Result<LocalBoxModelEntry> {
    let catalog = read_models_catalog().await?;
    catalog_entry(&catalog, name).cloned().ok_or_else(|| {
        let known = catalog
            .models
            .iter()
            .map(|model| model.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::anyhow!("unknown LocalBox model '{name}'. Available names: {known}")
    })
}

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
    let serve_entry = match serve.as_deref() {
        Some(model) => Some(preflight_serve(model).await?),
        None => None,
    };
    let allow_untuned = if let Some(entry) = serve_entry
        .as_ref()
        .filter(|entry| entry.run_profile.source != "tuned")
    {
        let warning = entry.run_profile.warning.clone().unwrap_or_else(|| {
            format!(
                "Warning: no usable tuned profile for '{}' at {}; continuing uses LocalBox defaults.",
                entry.name, entry.run_profile.source_path
            )
        });
        eprintln!("{warning}");
        if assume_yes {
            true
        } else if stdin_is_tty {
            crate::models_cmd::confirm("continue once with LocalBox defaults?")?
        } else {
            anyhow::bail!(
                "an untuned launch needs an explicit choice — configure LocalBench or re-run interactively"
            )
        }
    } else {
        false
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

    // A direct serve target is authoritative: reuse only the same serving
    // identity; a different running model is replaced, then adopted.
    let mut state = detect().await;
    let target_is_running = serve_entry.as_ref().is_some_and(|entry| {
        matches!(
            &state,
            LocalBoxState::Running { model: Some(active), .. }
                if active == &entry.name || entry.aliases.iter().any(|alias| alias == active)
        )
    });
    if let Some(entry) = serve_entry.as_ref().filter(|_| !target_is_running) {
        if matches!(state, LocalBoxState::NotInstalled) {
            anyhow::bail!("LocalBox is not installed (no `localbox` on PATH)");
        }
        let model = &entry.name;
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
        let _ = start_localbox_serve(model, allow_untuned, ServeStdio::Inherit, None).await?;
        state = detect().await;
    } else if serve_entry.is_none() && !matches!(state, LocalBoxState::Running { .. }) {
        match state {
            LocalBoxState::NotInstalled => {
                anyhow::bail!("LocalBox is not installed (no `localbox` on PATH)")
            }
            LocalBoxState::InstalledNotRunning => anyhow::bail!(
                "no running LocalBox server found — run `/localbox serve <model>` or `localbox serve <model>` first"
            ),
            LocalBoxState::Running { .. } => {}
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
    allow_untuned: bool,
    stdio: ServeStdio,
    cancel: Option<&CancellationToken>,
) -> anyhow::Result<ServeWait> {
    let program = localbox_on_path()
        .ok_or_else(|| anyhow::anyhow!("LocalBox is not installed (no `localbox` on PATH)"))?;
    let mut command = tokio::process::Command::new(program);
    command
        .args(localbox_serve_args(model, allow_untuned))
        .kill_on_drop(false);
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

fn localbox_serve_args(model: &str, allow_untuned: bool) -> Vec<String> {
    let mut args = vec!["serve".to_string(), model.to_string()];
    if allow_untuned {
        args.push("--allow-untuned".to_string());
    }
    args
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
    allow_untuned: bool,
    profile: Profile,
    trusted: bool,
    consent: TerminalConsent<'_>,
    cancel: &CancellationToken,
) -> anyhow::Result<TerminalAdoptOutcome> {
    let serve_entry = match serve {
        Some(model) => Some(preflight_serve(model).await?),
        None => None,
    };
    if let Some(entry) = serve_entry
        .as_ref()
        .filter(|entry| entry.run_profile.source != "tuned")
        .filter(|_| !allow_untuned)
    {
        let warning = entry.run_profile.warning.clone().unwrap_or_else(|| {
            format!(
                "Warning: no usable tuned profile for '{}' at {}; continuing uses LocalBox defaults.",
                entry.name, entry.run_profile.source_path
            )
        });
        anyhow::bail!(
            "{warning}\nNo model was started. Configure tuned settings, or explicitly continue once with `/localbox serve {} --allow-untuned`.",
            entry.name
        );
    }
    let canonical_serve = serve_entry.as_ref().map(|entry| entry.name.as_str());
    let request = TerminalAdoptRequest {
        cwd,
        serve: canonical_serve,
        profile,
        trusted,
        consent,
        cancel,
    };
    run_terminal_adopt_with(request, detect, |model, operation_cancel| async move {
        start_localbox_serve(
            &model,
            allow_untuned,
            ServeStdio::Null,
            Some(&operation_cancel),
        )
        .await
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
    let target_is_running = request.serve.is_some_and(|target| {
        matches!(
            &state,
            LocalBoxState::Running { model: Some(active), .. } if active == target
        )
    });
    if let Some(model) = request.serve.filter(|_| !target_is_running) {
        if matches!(state, LocalBoxState::NotInstalled) {
            anyhow::bail!("LocalBox is not installed (no `localbox` on PATH)");
        }
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
    } else if request.serve.is_none() && !matches!(state, LocalBoxState::Running { .. }) {
        match state {
            LocalBoxState::NotInstalled => {
                anyhow::bail!("LocalBox is not installed (no `localbox` on PATH)")
            }
            LocalBoxState::InstalledNotRunning => anyhow::bail!(
                "no running LocalBox server found — use `/localbox serve <model>` or run `localbox serve <model>` first"
            ),
            LocalBoxState::Running { .. } => {}
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
/// agnostic write half of adopt**: the CLI (`run_adopt`) and the in-TUI `/localbox`
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
    fn schema_one_catalog_resolves_aliases_and_renders_key_first() {
        let catalog = parse_models_catalog(
            br#"{
                "schema":1,
                "models":[{
                    "name":"q36apex",
                    "aliases":["apex"],
                    "display_name":"Qwen APEX",
                    "repository":"owner/apex",
                    "default_quant":"iq4",
                    "required_mode":"native",
                    "run_profile":{
                        "source":"tuned",
                        "source_path":"C:/profiles/best-q36apex.json",
                        "quant":"iq3",
                        "context":"64k",
                        "mode":"native"
                    }
                }]
            }"#,
        )
        .unwrap();
        assert_eq!(catalog_entry(&catalog, "apex").unwrap().name, "q36apex");
        let rendered = render_models_catalog(&catalog, Some("q36apex"));
        assert!(rendered.contains(
            "q36apex (aliases: apex) [active] — Qwen APEX · owner/apex · quant iq4 · mode native · tuned iq3 / 64k / native"
        ));
        assert!(rendered.ends_with("/localbox serve <name>\n"));
    }

    #[test]
    fn catalog_contract_rejects_unknown_schema_and_sanitizes_fields() {
        let err = parse_models_catalog(br#"{"schema":2,"models":[]}"#).unwrap_err();
        assert!(err.to_string().contains("schema 2"));
        assert_eq!(safe_catalog_field("ok\u{1b}[31m\nnext", 20), "ok[31mnext");
    }

    #[test]
    fn untuned_child_flag_is_forwarded_only_for_the_explicit_retry() {
        assert_eq!(localbox_serve_args("apex", false), ["serve", "apex"]);
        assert_eq!(
            localbox_serve_args("apex", true),
            ["serve", "apex", "--allow-untuned"]
        );
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
    async fn direct_serve_replaces_a_different_running_model_before_adoption() {
        use std::cell::RefCell;
        use std::collections::VecDeque;

        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let states = RefCell::new(VecDeque::from([
            LocalBoxState::Running {
                endpoint: DEFAULT_PROXY_BASE_URL.to_string(),
                model: Some("old-model".to_string()),
            },
            LocalBoxState::Running {
                endpoint: DEFAULT_PROXY_BASE_URL.to_string(),
                model: Some("new-model".to_string()),
            },
        ]));
        let started = RefCell::new(Vec::new());
        let outcome = run_terminal_adopt_with(
            TerminalAdoptRequest {
                cwd: dir.path(),
                serve: Some("new-model"),
                profile: Profile::Unrestricted,
                trusted: true,
                consent: TerminalConsent::ExplicitCommand,
                cancel: &cancel,
            },
            || std::future::ready(states.borrow_mut().pop_front().unwrap()),
            |model, _cancel| {
                started.borrow_mut().push(model);
                std::future::ready(Ok(ServeWait::Complete))
            },
        )
        .await
        .unwrap();

        assert!(matches!(outcome, TerminalAdoptOutcome::Adopted(_)));
        assert_eq!(started.into_inner(), vec!["new-model"]);
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
