//! `localpilot models` — list the models configured local servers actually
//! have loaded, via the OpenAI-compatible `GET /models` listing.

use std::io::Write as _;

use localpilot_config::{CliOverrides, Config, ConfigPaths, ProviderConfig};
use localpilot_sandbox::{Decision, Effect, Interactivity, PermissionEngine, PermissionRequest};

/// Run model discovery against every compatible configured provider (or one
/// named provider) and print the result.
///
/// # Errors
/// Returns an error if configuration cannot be loaded or output cannot be
/// written.
pub async fn run(provider_filter: Option<&str>) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let config = localpilot_config::load(&ConfigPaths::standard(&cwd), &CliOverrides::default())?;
    let engine = PermissionEngine::new(profile(&config), Vec::new());
    let mut stdout = std::io::stdout();

    let mut any = false;
    for (id, entry) in &config.providers {
        if provider_filter.is_some_and(|filter| filter != id) {
            continue;
        }
        let Some(base_url) = listing_base_url(entry) else {
            continue;
        };
        any = true;

        // Discovery is a network effect like any other: it passes the
        // permission engine before a request leaves the machine.
        let request = PermissionRequest {
            tool: "models".to_string(),
            effect: Effect::Network,
            interactivity: Interactivity::Interactive,
            trusted: true,
            detail: format!("{base_url}/models"),
        };
        let allowed = match engine.decide(&request) {
            Decision::Allow => true,
            Decision::Ask => confirm(&format!("query {} for its model list?", request.detail))?,
            Decision::Deny => false,
        };
        if !allowed {
            writeln!(stdout, "{id}: skipped (network request not approved)")?;
            continue;
        }

        let credential = config.resolve_credential(id);
        match localpilot_llm::discover_models(&base_url, credential.as_ref()).await {
            Ok(models) if models.is_empty() => {
                writeln!(stdout, "{id}: no models loaded")?;
            }
            Ok(models) => {
                writeln!(stdout, "{id} ({base_url}):")?;
                for model in models {
                    let configured = entry.model.as_deref() == Some(model.id.as_str());
                    let marker = if configured { "  * " } else { "    " };
                    match model.context_window {
                        Some(window) => {
                            writeln!(stdout, "{marker}{} (context {window})", model.id)?;
                        }
                        None => writeln!(stdout, "{marker}{}", model.id)?,
                    }
                }
            }
            Err(err) => writeln!(stdout, "{id}: unreachable ({err})")?,
        }
    }

    if !any {
        writeln!(
            stdout,
            "no providers with an OpenAI-compatible model listing are configured"
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
        _ => None,
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
