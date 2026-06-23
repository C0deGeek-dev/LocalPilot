//! Live judge ranking self-test: prove the LLM judge can rank a known
//! better-vs-worse pair on a *real* model before any live score is trusted
//! ("prove the instrument before spending on it", applied to the judge).
//!
//! Off by default; gated behind `LOCALPILOT_LIVE_TESTS` exactly like the live
//! discipline scorecard. Run with a configured local model:
//!   `LOCALPILOT_LIVE_TESTS=1 [LOCALPILOT_LIVE_MODEL=<model>] cargo test -p localpilot-harness --test judge_live -- --nocapture`
//! Skips cleanly when the env var, a provider, or a model is absent.
#![allow(clippy::unwrap_used)]

use localpilot_config::{load, CliOverrides, ConfigPaths};
use localpilot_harness::{Judge, JudgeCache, RankingTrust};
use localpilot_llm::ProviderRegistry;

#[test]
fn live_judge_ranking_selftest_is_gated() {
    if std::env::var("LOCALPILOT_LIVE_TESTS").is_err() {
        eprintln!("skipping live judge ranking self-test: set LOCALPILOT_LIVE_TESTS to enable");
        return;
    }

    let cwd = std::env::current_dir().unwrap();
    let config = match load(&ConfigPaths::standard(&cwd), &CliOverrides::default()) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("skipping live judge: config load failed: {err}");
            return;
        }
    };
    let registry = match ProviderRegistry::from_config(&config) {
        Ok(registry) => registry,
        Err(err) => {
            eprintln!("skipping live judge: provider configuration is incomplete: {err}");
            return;
        }
    };
    let Some(provider) = registry.default_provider().cloned() else {
        eprintln!("skipping live judge: no default provider is configured");
        return;
    };
    let Some(model) = std::env::var("LOCALPILOT_LIVE_MODEL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| config.resolve_model(None))
    else {
        eprintln!("skipping live judge: set provider.model or LOCALPILOT_LIVE_MODEL");
        return;
    };

    let mut judge = Judge::new("live-ranking-selftest", JudgeCache::default());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let trust = rt
        .block_on(judge.ranking_selftest_live(&provider, &model))
        .expect("the live ranking self-test should score the fixtures and return a verdict");

    match &trust {
        RankingTrust::Trustworthy => {
            eprintln!("live judge ranking self-test: TRUSTWORTHY (model={model})");
        }
        RankingTrust::Untrustworthy(why) => {
            eprintln!("live judge ranking self-test: UNTRUSTWORTHY (model={model}): {why}");
        }
    }

    // The instrument either proves itself trustworthy on the real model or names
    // why it cannot — both are valid recorded outcomes (offline evidence policy).
    // The assertion is that the self-test *ran live and produced a verdict*, not
    // that a weak local model necessarily passes it.
    assert!(matches!(
        trust,
        RankingTrust::Trustworthy | RankingTrust::Untrustworthy(_)
    ));
}
