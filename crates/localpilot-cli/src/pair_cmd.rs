//! Argument parsing and preflight resolution for two-agent collaboration.

use std::io::Write;
use std::time::Duration;

use anyhow::{anyhow, Result};
use clap::Args;
use localpilot_config::{CliOverrides, Config, ConfigPaths};
use localpilot_sandbox::Profile;
use localpilot_server::swarm::PairBounds;
use localpilot_store::Store;
use localpilot_terminal_ui::{Header, SessionHeader};
use localpilot_tui::Mode;

use crate::interactive_session::{
    InteractivePairHost, InteractivePeerSelection, InteractiveSessionSetup, PairPeer,
};
use crate::pair_run::{PairRunSetupFailure, PreparedPairRun};

pub(crate) const PAIR_ABOUT: &str = "Run an opt-in two-agent collaboration. Both peers use the configured default provider and model unless selected separately.";
pub(crate) const PAIR_EXIT_HELP: &str = "Exit status 0 means the agents converged. A round cap, timeout, abort, peer or provider failure, protocol error, budget limit, no progress, or driver failure returns a nonzero status.";

const DEFAULT_MAX_ROUNDS: u32 = 3;
const DEFAULT_SLOT_TIMEOUT_SECS: u64 = 600;

#[derive(Debug, Args)]
pub(crate) struct PairArgs {
    /// Task for both agents to collaborate on.
    pub(crate) task: String,
    /// Provider for peer A; defaults to the configured default provider.
    #[arg(long, value_name = "PROVIDER")]
    provider_a: Option<String>,
    /// Model for peer A; defaults to that provider's configured model.
    #[arg(long, value_name = "MODEL")]
    model_a: Option<String>,
    /// Provider for peer B; defaults to the configured default provider.
    #[arg(long, value_name = "PROVIDER")]
    provider_b: Option<String>,
    /// Model for peer B; defaults to that provider's configured model.
    #[arg(long, value_name = "MODEL")]
    model_b: Option<String>,
    /// Permission profile shared by both agents (default | relaxed | bypass | unrestricted).
    #[arg(long, value_name = "PROFILE")]
    permission: Option<String>,
    /// Shorthand for `--permission bypass`. Must be set explicitly.
    #[arg(long)]
    bypass: bool,
    /// Maximum collaboration rounds. Defaults to 3.
    #[arg(long, default_value_t = DEFAULT_MAX_ROUNDS)]
    max_rounds: u32,
    /// Maximum duration of one agent slot, in seconds. Defaults to 600.
    #[arg(
        long,
        value_name = "SECONDS",
        default_value_t = DEFAULT_SLOT_TIMEOUT_SECS
    )]
    slot_timeout: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ResolvedPeer {
    pub(crate) provider_id: String,
    pub(crate) model: String,
}

#[derive(Debug)]
pub(crate) struct ResolvedPairArgs {
    pub(crate) task: String,
    pub(crate) a: ResolvedPeer,
    pub(crate) b: ResolvedPeer,
    pub(crate) profile: Profile,
    pub(crate) bounds: PairBounds,
}

impl PairArgs {
    /// Resolve defaults and reject invalid selections before session setup begins.
    pub(crate) fn resolve(self, config: &Config) -> Result<ResolvedPairArgs> {
        if self.task.trim().is_empty() {
            return Err(anyhow!("collaboration task must not be empty"));
        }
        if self.max_rounds == 0 {
            return Err(anyhow!("maximum rounds must be greater than zero"));
        }
        if self.slot_timeout == 0 {
            return Err(anyhow!("slot timeout must be greater than zero"));
        }

        let a = resolve_peer(config, PairPeer::A, self.provider_a, self.model_a)?;
        let b = resolve_peer(config, PairPeer::B, self.provider_b, self.model_b)?;

        Ok(ResolvedPairArgs {
            task: self.task,
            a,
            b,
            profile: crate::session_cmd::resolve_profile(self.permission.as_deref(), self.bypass),
            bounds: PairBounds {
                max_rounds: self.max_rounds,
                slot_timeout: Duration::from_secs(self.slot_timeout),
                slot_token_budget: 0,
            },
        })
    }
}

/// Write the pre-run cost/quota note for a resolved pair run. Kept pure so it can be
/// asserted byte-for-byte; the caller flushes it before any prune, setup, session, or
/// model work. It states the honest bounds and never promises a fixed cost multiplier.
pub(crate) fn write_cost_notice(
    out: &mut dyn Write,
    resolved: &ResolvedPairArgs,
) -> std::io::Result<()> {
    writeln!(
        out,
        "localpilot pair: starts two resident agent histories, so it may use more tokens and provider quota than a single session. Only one model turn runs at a time. This run is bounded to {} rounds and {} seconds per slot; /abort or Ctrl+C stops it. These bounds do not promise a fixed token or price multiplier.",
        resolved.bounds.max_rounds,
        resolved.bounds.slot_timeout.as_secs(),
    )
}

pub(crate) async fn run(args: PairArgs) -> Result<crate::repl::ChatOutcome> {
    let cwd = std::env::current_dir()?;
    let config = localpilot_config::load(&ConfigPaths::standard(&cwd), &CliOverrides::default())?;
    let resolved = args.resolve(&config)?;

    // Surface the required cost/quota note on stderr before any prune, setup, session,
    // or model work begins. The disclosure is mandatory, so a failed write fails the
    // preflight rather than starting two sessions without it.
    {
        let stderr = std::io::stderr();
        let mut handle = stderr.lock();
        write_cost_notice(&mut handle, &resolved)?;
        handle.flush()?;
    }

    if config.storage.auto_prune {
        let policy = crate::session_cmd::retention_policy(&config.storage, None, None);
        if !policy.is_unbounded() {
            let _ = Store::open(&cwd).prune(policy, crate::session_cmd::now_unix(), false);
        }
    }

    let setup = InteractiveSessionSetup::resolve(cwd.clone(), config, resolved.profile).await?;
    let host = InteractivePairHost::prepare(
        &setup,
        &resolved.task,
        InteractivePeerSelection {
            provider_id: &resolved.a.provider_id,
            model: &resolved.a.model,
        },
        InteractivePeerSelection {
            provider_id: &resolved.b.provider_id,
            model: &resolved.b.model,
        },
    )
    .await?;
    let sessions = host.sessions();
    let prepared = match PreparedPairRun::new(host, resolved.bounds) {
        Ok(prepared) => prepared,
        Err(failure) => return close_setup_failure(failure).await,
    };

    let git = crate::repl::workspace_git_status(&cwd);
    let profile = crate::repl::ui_profile(resolved.profile)
        .label()
        .to_string();
    let primary = Header {
        version: env!("LOCALPILOT_VERSION").to_string(),
        provider: resolved.a.provider_id,
        model: resolved.a.model,
        workspace: cwd.display().to_string(),
        branch: git.as_ref().map(|status| status.branch.clone()),
        workspace_dirty: git.as_ref().and_then(|status| status.dirty),
        mode: Mode::Agent.label().to_string(),
        profile,
        session_id: sessions[0].to_string(),
        session_name: None,
    };
    let secondary = SessionHeader {
        provider: resolved.b.provider_id,
        model: resolved.b.model,
        session_id: sessions[1].to_string(),
        session_name: None,
    };
    let history =
        localpilot_store::PromptHistory::new(setup.config().history.persistence.is_enabled());
    let trust_required = !matches!(resolved.profile, Profile::Bypass | Profile::Unrestricted)
        && !crate::trust::is_trusted(&cwd);
    let exit = crate::fullscreen::run_pair(
        primary,
        secondary,
        prepared,
        crate::fullscreen::PairHostContext {
            cwd: &cwd,
            history: &history,
            ingest: &setup.config().ingest,
            config: setup.config(),
            trust_required,
        },
    )
    .await?;
    Ok(crate::repl::ChatOutcome {
        succeeded: exit.converged && !exit.trust_denied,
        presentation: None,
    })
}

async fn close_setup_failure(failure: PairRunSetupFailure) -> Result<crate::repl::ChatOutcome> {
    let (issue, host) = failure.into_parts();
    host.close().await;
    Err(issue.into())
}

fn resolve_peer(
    config: &Config,
    peer: PairPeer,
    provider: Option<String>,
    model: Option<String>,
) -> Result<ResolvedPeer> {
    let provider_id = provider.unwrap_or_else(|| config.provider.default.clone());
    validate_selection(peer, "provider", &provider_id)?;
    if !config.providers.contains_key(&provider_id) {
        return Err(anyhow!(
            "peer {} provider '{}' is not configured",
            peer.label(),
            provider_id
        ));
    }

    let model = match model {
        Some(model) => model,
        None => config.resolve_model(Some(&provider_id)).ok_or_else(|| {
            let model_flag = match peer {
                PairPeer::A => "--model-a",
                PairPeer::B => "--model-b",
            };
            anyhow!(
                "peer {} provider '{}' has no configured model; pass {model_flag} or configure one",
                peer.label(),
                provider_id
            )
        })?,
    };
    validate_selection(peer, "model", &model)?;

    Ok(ResolvedPeer { provider_id, model })
}

fn validate_selection(peer: PairPeer, kind: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(anyhow!("peer {} {kind} must not be empty", peer.label()));
    }
    if value.trim() != value {
        return Err(anyhow!(
            "peer {} {kind} must not have leading or trailing whitespace",
            peer.label()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};
    use localpilot_config::ProviderConfig;

    #[derive(Debug, Parser)]
    #[command(name = "pair", about = PAIR_ABOUT, after_long_help = PAIR_EXIT_HELP)]
    struct PairParser {
        #[command(flatten)]
        args: PairArgs,
    }

    fn parse(args: &[&str]) -> PairArgs {
        PairParser::try_parse_from(args).unwrap().args
    }

    fn configured() -> Config {
        let mut config = Config::default();
        config.provider.default = "first".to_string();
        config.providers.insert(
            "first".to_string(),
            ProviderConfig {
                model: Some("model-a".to_string()),
                ..ProviderConfig::default()
            },
        );
        config.providers.insert(
            "second".to_string(),
            ProviderConfig {
                model: Some("model-b".to_string()),
                ..ProviderConfig::default()
            },
        );
        config
    }

    #[test]
    fn defaults_both_peers_to_the_configured_provider_and_model() {
        let resolved = parse(&["pair", "review the change"])
            .resolve(&configured())
            .unwrap();

        assert_eq!(resolved.task, "review the change");
        assert_eq!(
            resolved.a,
            ResolvedPeer {
                provider_id: "first".to_string(),
                model: "model-a".to_string(),
            }
        );
        assert_eq!(resolved.a, resolved.b);
        assert_eq!(resolved.profile, Profile::Default);
        assert_eq!(resolved.bounds.max_rounds, DEFAULT_MAX_ROUNDS);
        assert_eq!(
            resolved.bounds.slot_timeout,
            Duration::from_secs(DEFAULT_SLOT_TIMEOUT_SECS)
        );
        assert_eq!(resolved.bounds.slot_token_budget, 0);
    }

    #[test]
    fn resolves_each_provider_default_independently() {
        let resolved = parse(&[
            "pair",
            "compare approaches",
            "--provider-a",
            "second",
            "--provider-b",
            "first",
            "--permission",
            "relaxed",
        ])
        .resolve(&configured())
        .unwrap();

        assert_eq!(resolved.a.provider_id, "second");
        assert_eq!(resolved.a.model, "model-b");
        assert_eq!(resolved.b.provider_id, "first");
        assert_eq!(resolved.b.model, "model-a");
        assert_eq!(resolved.profile, Profile::Relaxed);
    }

    #[test]
    fn accepts_explicit_models_without_a_catalog_gate() {
        let resolved = parse(&[
            "pair",
            "compare approaches",
            "--model-a",
            "unlisted-a",
            "--model-b",
            "unlisted-b",
            "--bypass",
            "--max-rounds",
            "7",
            "--slot-timeout",
            "45",
        ])
        .resolve(&configured())
        .unwrap();

        assert_eq!(resolved.a.model, "unlisted-a");
        assert_eq!(resolved.b.model, "unlisted-b");
        assert_eq!(resolved.profile, Profile::Bypass);
        assert_eq!(resolved.bounds.max_rounds, 7);
        assert_eq!(resolved.bounds.slot_timeout, Duration::from_secs(45));
    }

    #[test]
    fn rejects_an_unconfigured_provider() {
        let error = parse(&["pair", "task", "--provider-b", "missing"])
            .resolve(&configured())
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "peer B provider 'missing' is not configured"
        );
    }

    #[test]
    fn rejects_a_provider_without_a_default_model() {
        let mut config = configured();
        config.providers.get_mut("second").unwrap().model = None;
        let error = parse(&["pair", "task", "--provider-a", "second"])
            .resolve(&config)
            .unwrap_err();

        assert_eq!(
            error.to_string(),
            "peer A provider 'second' has no configured model; pass --model-a or configure one"
        );
    }

    #[test]
    fn rejects_blank_and_padded_selections() {
        for (args, expected) in [
            (
                vec!["pair", "task", "--provider-a", ""],
                "peer A provider must not be empty",
            ),
            (
                vec!["pair", "task", "--provider-a", " first"],
                "peer A provider must not have leading or trailing whitespace",
            ),
            (
                vec!["pair", "task", "--model-b", "model-b "],
                "peer B model must not have leading or trailing whitespace",
            ),
            (
                vec!["pair", "task", "--model-b", ""],
                "peer B model must not be empty",
            ),
        ] {
            let error = parse(&args).resolve(&configured()).unwrap_err();
            assert_eq!(error.to_string(), expected);
        }
    }

    #[test]
    fn rejects_a_blank_task_and_zero_bounds() {
        for (args, expected) in [
            (vec!["pair", "   "], "collaboration task must not be empty"),
            (
                vec!["pair", "task", "--max-rounds", "0"],
                "maximum rounds must be greater than zero",
            ),
            (
                vec!["pair", "task", "--slot-timeout", "0"],
                "slot timeout must be greater than zero",
            ),
        ] {
            let error = parse(&args).resolve(&configured()).unwrap_err();
            assert_eq!(error.to_string(), expected);
        }
    }

    #[test]
    fn preserves_a_nonblank_task_exactly() {
        let resolved = parse(&["pair", "  keep my spacing  "])
            .resolve(&configured())
            .unwrap();

        assert_eq!(resolved.task, "  keep my spacing  ");
    }

    #[test]
    fn the_cost_notice_is_exact_bounds_aware_and_promises_no_multiplier() {
        let resolved = parse(&["pair", "task", "--max-rounds", "5", "--slot-timeout", "120"])
            .resolve(&configured())
            .unwrap();
        let mut buffer = Vec::new();
        write_cost_notice(&mut buffer, &resolved).unwrap();
        assert_eq!(
            String::from_utf8(buffer).unwrap(),
            "localpilot pair: starts two resident agent histories, so it may use more tokens and \
provider quota than a single session. Only one model turn runs at a time. This run is bounded to 5 \
rounds and 120 seconds per slot; /abort or Ctrl+C stops it. These bounds do not promise a fixed \
token or price multiplier.\n"
        );
    }

    #[test]
    fn help_discloses_defaults_and_the_complete_staged_surface() {
        let help = PairParser::command().render_long_help().to_string();

        assert!(help.contains(PAIR_ABOUT));
        assert!(help.contains(PAIR_EXIT_HELP));
        for expected in [
            "--provider-a <PROVIDER>",
            "--model-a <MODEL>",
            "--provider-b <PROVIDER>",
            "--model-b <MODEL>",
            "--permission <PROFILE>",
            "--bypass",
            "--max-rounds <MAX_ROUNDS>",
            "[default: 3]",
            "--slot-timeout <SECONDS>",
            "[default: 600]",
        ] {
            assert!(help.contains(expected), "missing help fragment: {expected}");
        }
        assert!(!help.contains("slot-token"));
        assert!(!help.contains("operating mode"));
    }
}
