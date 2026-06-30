//! `localpilot models` — list the models configured local servers actually
//! have loaded, via the OpenAI-compatible `GET /models` listing.
//!
//! Agent-consumable: it never prompts non-interactively (it reports
//! approval-required instead of silently skipping), emits JSON under the
//! ADR-0048 `--format` contract, and exits non-zero when a queried endpoint is
//! unreachable or approval is required without `--yes`.

use std::io::Write as _;

use localpilot_config::{CliOverrides, Config, ConfigPaths, ProviderConfig};
use localpilot_llm::resolve_vision_with_source;
use localpilot_sandbox::{Decision, Effect, Interactivity, PermissionEngine, PermissionRequest};
use serde::Serialize;

use crate::output::OutputFormat;

/// The terminal state of a `models` run a caller can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModelsOutcome {
    /// A queried endpoint was unreachable, or approval was required but the run
    /// was non-interactive without `--yes` — either way the listing is
    /// incomplete, so the caller should treat the run as failed.
    pub had_failure: bool,
}

/// Why a provider produced no model list (or did).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    /// Models were listed.
    Ok,
    /// The endpoint answered but reported no loaded models.
    NoModels,
    /// The endpoint could not be reached.
    Unreachable,
    /// Network approval was required but the run was non-interactive without
    /// `--yes`, so the request was not sent (reported, never silently skipped).
    ApprovalRequired,
    /// The permission policy denied the network request.
    Denied,
    /// The provider speaks a protocol with no `GET /models` listing.
    NoListingEndpoint,
}

#[derive(Debug, Clone, Serialize)]
struct ModelEntry {
    id: String,
    context_window: Option<u64>,
    /// Whether this is the provider's configured default model.
    configured: bool,
}

#[derive(Debug, Clone, Serialize)]
struct ProviderModels {
    provider: String,
    kind: String,
    base_url: Option<String>,
    status: Status,
    models: Vec<ModelEntry>,
    /// The provider's declared vision (image-input) capability, when set in
    /// config. The authoritative half of the resolution (config > probe > false).
    #[serde(skip_serializing_if = "Option::is_none")]
    supports_vision: Option<bool>,
    /// The resolved vision capability (config > probe > false) for a reachable
    /// server. `None` when the server was not reached or not probed.
    #[serde(skip_serializing_if = "Option::is_none")]
    vision: Option<bool>,
    /// Which signal decided `vision`: `config`, `probe`, or `default`. `None`
    /// alongside a `None` `vision` when nothing was resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    vision_source: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Run model discovery against every compatible configured provider (or one
/// named provider) and print the result in the requested format.
///
/// `assume_yes` approves the network request without prompting; `stdin_is_tty`
/// gates interactive prompting — a non-interactive run never blocks on a prompt.
///
/// # Errors
/// Returns an error if configuration cannot be loaded or output cannot be
/// written.
pub async fn run(
    provider_filter: Option<&str>,
    format: OutputFormat,
    assume_yes: bool,
    stdin_is_tty: bool,
) -> anyhow::Result<ModelsOutcome> {
    let cwd = std::env::current_dir()?;
    let config = localpilot_config::load(&ConfigPaths::standard(&cwd), &CliOverrides::default())?;
    let engine = PermissionEngine::new(profile(&config), Vec::new());

    let mut results: Vec<ProviderModels> = Vec::new();
    for (id, entry) in &config.providers {
        if provider_filter.is_some_and(|filter| filter != id) {
            continue;
        }
        let Some(base_url) = listing_base_url(entry) else {
            results.push(ProviderModels {
                provider: id.clone(),
                kind: entry.kind.clone(),
                base_url: entry.base_url.clone(),
                status: Status::NoListingEndpoint,
                models: Vec::new(),
                supports_vision: entry.supports_vision,
                vision: None,
                vision_source: None,
                error: None,
            });
            continue;
        };

        // Network discovery is an effect: it passes the permission engine before a
        // request leaves the machine. A non-interactive run (no TTY, or `--yes`)
        // declares itself so, so an `Ask` is never a blocking stdin prompt.
        let interactivity = if stdin_is_tty && !assume_yes {
            Interactivity::Interactive
        } else {
            Interactivity::NonInteractive
        };
        let request = PermissionRequest {
            tool: "models".to_string(),
            effect: Effect::Network,
            interactivity,
            trusted: true,
            detail: format!("{base_url}/models"),
        };
        // `--yes` is explicit approval; otherwise consult the policy.
        let decision = if assume_yes {
            Decision::Allow
        } else {
            engine.decide(&request)
        };
        let approved = match decision {
            Decision::Allow => true,
            Decision::Ask => {
                if stdin_is_tty {
                    confirm(&format!("query {} for its model list?", request.detail))?
                } else {
                    // Non-interactive and approval is required: report it (a nonzero
                    // exit) rather than silently skip or hang on a prompt.
                    results.push(provider_blocked(
                        id,
                        entry,
                        &base_url,
                        Status::ApprovalRequired,
                    ));
                    continue;
                }
            }
            Decision::Deny => false,
        };
        if !approved {
            results.push(provider_blocked(id, entry, &base_url, Status::Denied));
            continue;
        }

        let entry_default = entry.model.as_deref();
        match discover_models_for_provider(&config, id, &base_url).await {
            Ok(models) if models.is_empty() => {
                results.push(provider_blocked(id, entry, &base_url, Status::NoModels));
            }
            Ok(models) => {
                let listed = models
                    .into_iter()
                    .map(|model| ModelEntry {
                        configured: entry_default == Some(model.id.as_str()),
                        id: model.id,
                        context_window: model.context_window,
                    })
                    .collect();
                // The server is reachable, so resolve its vision capability:
                // config wins, else a best-effort read-only probe, else default off.
                let probe = if config.discovery.vision_probe {
                    probe_vision_for_provider(&config, id, &base_url).await
                } else {
                    None
                };
                let (vision, source) = resolve_vision_with_source(entry.supports_vision, probe);
                results.push(ProviderModels {
                    provider: id.clone(),
                    kind: entry.kind.clone(),
                    base_url: Some(base_url.clone()),
                    status: Status::Ok,
                    models: listed,
                    supports_vision: entry.supports_vision,
                    vision: Some(vision),
                    vision_source: Some(source.as_str()),
                    error: None,
                });
            }
            Err(err) => {
                let mut blocked = provider_blocked(id, entry, &base_url, Status::Unreachable);
                blocked.error = Some(err.to_string());
                results.push(blocked);
            }
        }
    }

    let had_failure = listing_incomplete(&results);

    let mut stdout = std::io::stdout();
    match format {
        OutputFormat::Json => render_json(&mut stdout, &results)?,
        OutputFormat::Human => render_human(&mut stdout, &results)?,
    }
    Ok(ModelsOutcome { had_failure })
}

/// Whether the listing is incomplete and so the run should exit non-zero: an
/// endpoint was unreachable, or approval was required but the run was
/// non-interactive without `--yes` (so it was reported, not silently skipped). A
/// policy `Deny` or a `NoListingEndpoint` is a configuration fact, not a failure.
fn listing_incomplete(results: &[ProviderModels]) -> bool {
    results
        .iter()
        .any(|r| matches!(r.status, Status::Unreachable | Status::ApprovalRequired))
}

/// A provider entry that produced no models, for one of the non-`Ok` statuses.
fn provider_blocked(
    id: &str,
    entry: &ProviderConfig,
    base_url: &str,
    status: Status,
) -> ProviderModels {
    ProviderModels {
        provider: id.to_string(),
        kind: entry.kind.clone(),
        base_url: Some(base_url.to_string()),
        status,
        models: Vec::new(),
        supports_vision: entry.supports_vision,
        vision: None,
        vision_source: None,
        error: None,
    }
}

/// Best-effort, read-only vision probe for a provider's server (llama.cpp
/// `/props`). A dynamic-auth provider (e.g. Google Vertex) has no such endpoint,
/// so it is skipped; otherwise the configured static credential is used.
pub(crate) async fn probe_vision_for_provider(
    config: &Config,
    provider_id: &str,
    base_url: &str,
) -> Option<bool> {
    if localpilot_llm::discovery_auth_provider_from_config(config, provider_id)
        .ok()
        .flatten()
        .is_some()
    {
        return None;
    }
    let credential = config.resolve_credential(provider_id);
    localpilot_llm::probe_vision(base_url, credential.as_ref()).await
}

/// Emit the results as a JSON array (the agent-consumable surface). Stdout stays
/// script-stable: an empty result is a valid empty array.
fn render_json(out: &mut dyn std::io::Write, results: &[ProviderModels]) -> anyhow::Result<()> {
    let body = serde_json::to_string_pretty(results)?;
    writeln!(out, "{body}")?;
    Ok(())
}

/// Emit the human-readable listing. Mirrors the prior output, plus explicit lines
/// for the approval-required / denied / unreachable states and the unlistable
/// providers, so no outcome is silent.
fn render_human(out: &mut dyn std::io::Write, results: &[ProviderModels]) -> anyhow::Result<()> {
    let mut listed_any = false;
    let mut unlistable: Vec<String> = Vec::new();

    for r in results {
        match r.status {
            Status::Ok => {
                listed_any = true;
                let base = r.base_url.as_deref().unwrap_or("");
                // Prefer the resolved capability (config or probe) for a reachable
                // server; fall back to the config declaration otherwise.
                let vision = match (r.vision, r.vision_source) {
                    (Some(true), Some(source)) => format!(" [vision: yes ({source})]"),
                    (Some(false), Some(source)) => format!(" [vision: no ({source})]"),
                    _ => match r.supports_vision {
                        Some(true) => " [vision declared]".to_string(),
                        Some(false) => " [vision off]".to_string(),
                        None => String::new(),
                    },
                };
                writeln!(out, "{} ({base}):{vision}", r.provider)?;
                for model in &r.models {
                    let marker = if model.configured { "  * " } else { "    " };
                    match model.context_window {
                        Some(window) => writeln!(out, "{marker}{} (context {window})", model.id)?,
                        None => writeln!(out, "{marker}{}", model.id)?,
                    }
                }
            }
            Status::NoModels => {
                listed_any = true;
                writeln!(out, "{}: no models loaded", r.provider)?;
            }
            Status::Unreachable => {
                listed_any = true;
                let err = r.error.as_deref().unwrap_or("unreachable");
                writeln!(out, "{}: unreachable ({err})", r.provider)?;
            }
            Status::ApprovalRequired => {
                listed_any = true;
                writeln!(
                    out,
                    "{}: skipped — network approval required; pass --yes to query non-interactively",
                    r.provider
                )?;
            }
            Status::Denied => {
                listed_any = true;
                writeln!(
                    out,
                    "{}: skipped (network request denied by policy)",
                    r.provider
                )?;
            }
            Status::NoListingEndpoint => unlistable.push(format!("{} ({})", r.provider, r.kind)),
        }
    }

    if !listed_any && unlistable.is_empty() {
        writeln!(
            out,
            "no providers configured to list models — run `localpilot init` and configure one"
        )?;
    } else if !unlistable.is_empty() {
        writeln!(
            out,
            "no model listing for: {}. These providers don't expose a `GET /models` \
             endpoint, so the served model is whatever the local server has loaded — \
             set `[providers.<id>].model` in .localpilot.toml or query the server directly.",
            unlistable.join(", ")
        )?;
    }
    Ok(())
}

/// The base URL to query for a provider that speaks the OpenAI-compatible
/// listing, or `None` for protocol shapes without one. Shared with the `/model`
/// picker so both list models through the one discovery path.
pub(crate) fn listing_base_url(entry: &ProviderConfig) -> Option<String> {
    match entry.kind.as_str() {
        "openai" => Some(
            entry
                .base_url
                .clone()
                .or_else(|| env_non_empty("OPENAI_BASE_URL"))
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
        ),
        "openai-compatible" | "local" | "custom" | "custom-user-endpoint" => entry
            .base_url
            .clone()
            .or_else(|| env_non_empty("OPENAI_BASE_URL")),
        "google-vertex-openai" => entry.base_url.clone().or_else(|| {
            let project = entry.google_project.as_deref()?.trim();
            let location = entry.google_location.as_deref()?.trim();
            if project.is_empty() || location.is_empty() {
                return None;
            }
            Some(format!(
                "https://aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/endpoints/openapi"
            ))
        }),
        _ => None,
    }
}

pub(crate) async fn discover_models_for_provider(
    config: &Config,
    provider_id: &str,
    base_url: &str,
) -> Result<Vec<localpilot_llm::DiscoveredModel>, localpilot_llm::ProviderError> {
    if let Some(auth_provider) =
        localpilot_llm::discovery_auth_provider_from_config(config, provider_id)?
    {
        localpilot_llm::discover_models_with_auth_provider(base_url, auth_provider.as_ref()).await
    } else {
        let credential = config.resolve_credential(provider_id);
        localpilot_llm::discover_models(base_url, credential.as_ref()).await
    }
}

fn profile(config: &Config) -> localpilot_sandbox::Profile {
    match config.permissions.profile {
        localpilot_config::PermissionProfile::Default => localpilot_sandbox::Profile::Default,
        localpilot_config::PermissionProfile::Relaxed => localpilot_sandbox::Profile::Relaxed,
        localpilot_config::PermissionProfile::Bypass => localpilot_sandbox::Profile::Bypass,
    }
}

fn confirm(question: &str) -> anyhow::Result<bool> {
    let mut stdout = std::io::stdout();
    write!(stdout, "{question} [y/N] ")?;
    stdout.flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_entry(provider: &str, configured: &str) -> ProviderModels {
        ProviderModels {
            provider: provider.to_string(),
            kind: "openai-compatible".to_string(),
            base_url: Some("http://localhost:11435/v1".to_string()),
            status: Status::Ok,
            models: vec![ModelEntry {
                id: configured.to_string(),
                context_window: Some(131_072),
                configured: true,
            }],
            supports_vision: None,
            vision: None,
            vision_source: None,
            error: None,
        }
    }

    #[test]
    fn json_is_a_script_stable_array() {
        let mut out: Vec<u8> = Vec::new();
        render_json(&mut out, &[]).unwrap();
        assert_eq!(String::from_utf8(out).unwrap().trim(), "[]");
    }

    #[test]
    fn json_carries_status_and_configured_marker() {
        let mut out: Vec<u8> = Vec::new();
        render_json(&mut out, &[ok_entry("local", "q3635ba3bapex")]).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        assert_eq!(parsed[0]["provider"], "local");
        assert_eq!(parsed[0]["status"], "ok");
        assert_eq!(parsed[0]["models"][0]["id"], "q3635ba3bapex");
        assert_eq!(parsed[0]["models"][0]["configured"], true);
    }

    #[test]
    fn json_carries_a_declared_vision_capability() {
        let mut entry = ok_entry("local", "q3635ba3bapex");
        entry.supports_vision = Some(true);
        let mut out: Vec<u8> = Vec::new();
        render_json(&mut out, &[entry]).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        assert_eq!(parsed[0]["supports_vision"], true);
        // An undeclared provider omits the field entirely (no guessed value).
        let mut out2: Vec<u8> = Vec::new();
        render_json(&mut out2, &[ok_entry("local", "m")]).unwrap();
        let parsed2: serde_json::Value =
            serde_json::from_str(&String::from_utf8(out2).unwrap()).unwrap();
        assert!(parsed2[0].get("supports_vision").is_none());
    }

    #[test]
    fn json_carries_the_resolved_vision_and_its_source() {
        // A reachable server probed as vision-capable surfaces the resolved value
        // and the signal that decided it, so a caller can see *why*.
        let mut entry = ok_entry("local", "m");
        entry.vision = Some(true);
        entry.vision_source = Some("probe");
        let mut out: Vec<u8> = Vec::new();
        render_json(&mut out, &[entry]).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        assert_eq!(parsed[0]["vision"], true);
        assert_eq!(parsed[0]["vision_source"], "probe");
    }

    #[test]
    fn an_unreachable_or_approval_required_endpoint_serializes_its_status() {
        let unreachable = ProviderModels {
            provider: "local".to_string(),
            kind: "local".to_string(),
            base_url: Some("http://localhost:9/v1".to_string()),
            status: Status::Unreachable,
            models: Vec::new(),
            supports_vision: None,
            vision: None,
            vision_source: None,
            error: Some("connection refused".to_string()),
        };
        let mut out: Vec<u8> = Vec::new();
        render_json(&mut out, &[unreachable]).unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&String::from_utf8(out).unwrap()).unwrap();
        assert_eq!(parsed[0]["status"], "unreachable");
        assert_eq!(parsed[0]["error"], "connection refused");
    }

    #[test]
    fn an_incomplete_listing_exits_nonzero_but_policy_facts_do_not() {
        // The offline guard for the agent contract: an unreachable endpoint or a
        // non-interactive approval-required state makes the run fail (non-zero exit);
        // a policy Deny or a no-listing-endpoint provider is a fact, not a failure.
        let mk = |status: Status| ProviderModels {
            provider: "p".to_string(),
            kind: "local".to_string(),
            base_url: Some("http://localhost:1/v1".to_string()),
            status,
            models: Vec::new(),
            supports_vision: None,
            vision: None,
            vision_source: None,
            error: None,
        };
        assert!(listing_incomplete(&[mk(Status::Unreachable)]));
        assert!(listing_incomplete(&[mk(Status::ApprovalRequired)]));
        assert!(!listing_incomplete(&[mk(Status::Denied)]));
        assert!(!listing_incomplete(&[mk(Status::NoListingEndpoint)]));
        assert!(!listing_incomplete(&[ok_entry("local", "m")]));
    }

    #[test]
    fn noninteractive_approval_required_is_reported_not_skipped_in_human_output() {
        let blocked = ProviderModels {
            provider: "local".to_string(),
            kind: "local".to_string(),
            base_url: Some("http://localhost:11435/v1".to_string()),
            status: Status::ApprovalRequired,
            models: Vec::new(),
            supports_vision: None,
            vision: None,
            vision_source: None,
            error: None,
        };
        let mut out: Vec<u8> = Vec::new();
        render_human(&mut out, &[blocked]).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("approval required"));
        assert!(text.contains("--yes"));
    }
}
