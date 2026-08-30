//! Live LLM-as-judge scoring on top of the shared blinded judge core.
//!
//! The judge's rubric, prompts, parsing, caching, and offline/gated scoring live
//! in `localx_eval_core::judge`; this module supplies the one piece that needs a
//! model provider — streaming a prompt to a live judge model and caching the raw
//! reply so a repeat (and any later offline run) is free and reproducible.
//! Opportunistic per the validation-evidence policy: the offline path is the
//! accepted CI bar; the live calls run only when a judge model is available.

use std::sync::Arc;

use futures::StreamExt;

use localpilot_core::{Message, Role};
use localpilot_llm::{ModelEvent, ModelProvider, ModelRequest};
use localx_eval_core::judge::{
    judge_prompt, parse_judge_block, Judge, JudgeBlock, JudgeError, JudgeInput, RankingTrust,
    RANKING_FIXTURES,
};

/// Score one solution with a live judge model, caching the response so a repeat
/// is free and offline-reproducible. A cache hit short-circuits the model call.
///
/// # Errors
/// Returns [`JudgeError`] if the provider cannot be reached/streamed or the
/// reply does not parse into the four scores.
pub async fn judge_score_live(
    judge: &mut Judge,
    provider: &Arc<dyn ModelProvider>,
    model: &str,
    input: &JudgeInput<'_>,
) -> Result<JudgeBlock, JudgeError> {
    if let Some(block) = judge.score_offline(input) {
        return Ok(block);
    }
    let prompt = judge_prompt(input);
    let raw = collect_completion(provider, model, &prompt).await?;
    judge.cache.insert(&prompt, raw.clone());
    parse_judge_block(&raw, &judge.judge_model, true).ok_or(JudgeError::Unparseable)
}

/// Live ranking self-test: judge the authored fixtures with a real model
/// (caching each response), then take the offline verdict over the now-warm
/// cache — "prove the instrument before spending on it", applied to the judge.
///
/// # Errors
/// Returns [`JudgeError`] if the provider cannot be reached/streamed or a
/// fixture reply does not parse.
pub async fn judge_ranking_selftest_live(
    judge: &mut Judge,
    provider: &Arc<dyn ModelProvider>,
    model: &str,
) -> Result<RankingTrust, JudgeError> {
    for fx in RANKING_FIXTURES {
        for diff in [fx.better, fx.worse] {
            judge_score_live(
                judge,
                provider,
                model,
                &JudgeInput {
                    diff,
                    trajectory: None,
                },
            )
            .await?;
        }
    }
    Ok(judge.ranking_selftest_offline())
}

/// Stream one prompt to the provider and collect the final answer text.
async fn collect_completion(
    provider: &Arc<dyn ModelProvider>,
    model: &str,
    prompt: &str,
) -> Result<String, JudgeError> {
    let request = ModelRequest::new(model, vec![Message::text(Role::User, prompt)]);
    let mut stream = provider
        .stream(request)
        .await
        .map_err(|e| JudgeError::Provider(e.to_string()))?;
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event {
            Ok(ModelEvent::TextDelta(delta)) => text.push_str(&delta),
            Ok(_) => {}
            Err(e) => return Err(JudgeError::Provider(e.to_string())),
        }
    }
    Ok(text)
}
