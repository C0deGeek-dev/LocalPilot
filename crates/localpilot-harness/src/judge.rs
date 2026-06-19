//! LLM-as-judge: a model scores the code-quality dimensions static signals
//! cannot see — readability, idiomatic style, the right abstraction, and
//! latent-bug risk — to complement the deterministic `quality` block.
//!
//! The discipline that makes the scores trustworthy is built in:
//! - **Blinded by construction.** Single-solution scoring embeds no arm identity
//!   in the prompt, so the judge cannot tell LocalPilot from a baseline. A
//!   comparative preference call presents the two solutions in a **seed-randomized
//!   order** and maps the verdict back, so order is not a tell.
//! - **Offline-deterministic.** [`Judge::score_offline`] answers from a cache
//!   keyed by the exact prompt, so CI never needs a model; the live path
//!   ([`Judge::score_live`]) is opportunistic and caches its response.
//! - **Calibrated.** [`cohens_kappa`] scores the judge against a human-labelled
//!   sample so agreement is reported, not assumed. The judge model must be
//!   stronger than the subject model (documented; the caller configures it).
//!
//! The rubric and prompt are original artefacts authored for this repository.

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use serde::{Deserialize, Serialize};

use localpilot_core::{Message, Role};
use localpilot_llm::{ModelEvent, ModelProvider, ModelRequest};

/// A judge failure on the live path.
#[derive(Debug, thiserror::Error)]
pub enum JudgeError {
    /// The judge provider could not be reached or streamed.
    #[error("judge provider error: {0}")]
    Provider(String),
    /// The judge replied but its scores could not be parsed.
    #[error("judge response could not be parsed into scores")]
    Unparseable,
    /// The judge failed its ranking self-test, so its scores are not trusted and
    /// scoring is refused before any spend. The string names the failed fixture.
    #[error("judge failed its ranking self-test: {0}")]
    Untrustworthy(String),
}

/// The original quality rubric: four dimensions, each scored `1..=5` with
/// higher always better (a low latent-bug risk scores *high* as `bug_resistance`).
pub const RUBRIC: &str = "\
Score the change on four dimensions, each an integer 1 to 5 where 5 is best:
- readability: is the code easy to follow? (5 = obvious intent, clear names; 1 = cryptic)
- idiomaticity: does it use the language and the surrounding code's conventions? (5 = idiomatic; 1 = fights the language)
- abstraction_fit: is the abstraction level right — neither over-engineered nor a copy-paste hack? (5 = right-sized; 1 = wrong level)
- bug_resistance: how free of latent bugs and unhandled edges is it? (5 = robust; 1 = fragile)";

/// The judge's per-dimension scores, recorded into the scorecard. Every
/// dimension is `1..=5`, higher is better; `overall` is their mean.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JudgeBlock {
    pub readability: u8,
    pub idiomaticity: u8,
    pub abstraction_fit: u8,
    pub bug_resistance: u8,
    pub overall: f64,
    /// The judge model id (or `cached`), so a report can show who judged.
    pub judge_model: String,
    /// Whether the scoring was blind to the solution's arm identity.
    pub blinded: bool,
}

impl JudgeBlock {
    /// Assemble a block from the four dimension scores, computing `overall`.
    #[must_use]
    pub fn from_dimensions(
        readability: u8,
        idiomaticity: u8,
        abstraction_fit: u8,
        bug_resistance: u8,
        judge_model: impl Into<String>,
        blinded: bool,
    ) -> Self {
        let overall = f64::from(
            u16::from(readability)
                + u16::from(idiomaticity)
                + u16::from(abstraction_fit)
                + u16::from(bug_resistance),
        ) / 4.0;
        Self {
            readability,
            idiomaticity,
            abstraction_fit,
            bug_resistance,
            overall,
            judge_model: judge_model.into(),
            blinded,
        }
    }
}

/// What the judge scores: the produced diff and, optionally, the trajectory that
/// produced it. No arm identity is included, so single-solution scoring is blind.
#[derive(Debug, Clone, Copy)]
pub struct JudgeInput<'a> {
    pub diff: &'a str,
    pub trajectory: Option<&'a str>,
}

/// Build the single-solution scoring prompt. It carries only the rubric and the
/// solution — never which harness arm produced it — so the score is blind.
#[must_use]
pub fn judge_prompt(input: &JudgeInput) -> String {
    let mut prompt = format!(
        "You are a meticulous senior code reviewer scoring one code change.\n\n{RUBRIC}\n\n\
         Reply with ONLY a JSON object of the four integer scores, e.g. \
         {{\"readability\":4,\"idiomaticity\":5,\"abstraction_fit\":4,\"bug_resistance\":3}}.\n\n\
         --- diff ---\n{}\n",
        input.diff
    );
    if let Some(trajectory) = input.trajectory {
        prompt.push_str(&format!("\n--- how it was produced ---\n{trajectory}\n"));
    }
    prompt
}

/// A pair of solutions presented for comparative judging, in a seed-randomized
/// order so the position is not a tell. `swapped` records whether `solution_1`
/// holds the *second* input (`b`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlindedPair {
    pub solution_1: String,
    pub solution_2: String,
    pub swapped: bool,
}

/// Present `a` and `b` in a deterministic, seed-randomized order. The same seed
/// always yields the same order, so an offline cache is reproducible.
#[must_use]
pub fn blind(a: &str, b: &str, seed: u64) -> BlindedPair {
    let swapped = fnv1a(&seed.to_le_bytes()) & 1 == 1;
    if swapped {
        BlindedPair {
            solution_1: b.to_string(),
            solution_2: a.to_string(),
            swapped,
        }
    } else {
        BlindedPair {
            solution_1: a.to_string(),
            solution_2: b.to_string(),
            swapped,
        }
    }
}

/// Build the comparative preference prompt for a blinded pair: the judge picks
/// the better solution by its presented position (1 or 2), knowing nothing of
/// which arm produced which.
#[must_use]
pub fn preference_prompt(pair: &BlindedPair) -> String {
    format!(
        "You are a meticulous senior code reviewer comparing two solutions to the \
         same task.\n\n{RUBRIC}\n\nReply with ONLY a JSON object naming the better \
         solution, e.g. {{\"preferred\":1}} or {{\"preferred\":2}}.\n\n\
         --- solution 1 ---\n{}\n\n--- solution 2 ---\n{}\n",
        pair.solution_1, pair.solution_2
    )
}

/// Which original arm a blinded preference verdict points to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preferred {
    /// The first input to [`blind`] (`a`).
    First,
    /// The second input to [`blind`] (`b`).
    Second,
}

/// Map a judge's presented-position preference (`1` or `2`) back to the original
/// arm, undoing the blinded swap. Returns `None` for an out-of-range position.
#[must_use]
pub fn resolve_preference(pair: &BlindedPair, presented: u8) -> Option<Preferred> {
    let first_is_a = !pair.swapped;
    match presented {
        1 if first_is_a => Some(Preferred::First),
        1 => Some(Preferred::Second),
        2 if first_is_a => Some(Preferred::Second),
        2 => Some(Preferred::First),
        _ => None,
    }
}

/// Parse a judge's JSON reply into a [`JudgeBlock`]. Tolerant of surrounding
/// prose: it reads the first `{...}` object. Returns `None` if the four scores
/// are not all present and in `1..=5`.
#[must_use]
pub fn parse_judge_block(raw: &str, judge_model: &str, blinded: bool) -> Option<JudgeBlock> {
    let value = first_json_object(raw)?;
    let dim = |key: &str| -> Option<u8> {
        let n = value.get(key)?.as_u64()?;
        u8::try_from(n).ok().filter(|s| (1..=5).contains(s))
    };
    Some(JudgeBlock::from_dimensions(
        dim("readability")?,
        dim("idiomaticity")?,
        dim("abstraction_fit")?,
        dim("bug_resistance")?,
        judge_model,
        blinded,
    ))
}

/// Parse a comparative preference reply into `1`/`2`.
#[must_use]
pub fn parse_preference(raw: &str) -> Option<u8> {
    let value = first_json_object(raw)?;
    let n = value.get("preferred")?.as_u64()?;
    u8::try_from(n).ok().filter(|p| *p == 1 || *p == 2)
}

/// Extract the first balanced `{...}` JSON object from arbitrary text.
fn first_json_object(raw: &str) -> Option<serde_json::Value> {
    let start = raw.find('{')?;
    let mut depth = 0usize;
    for (offset, ch) in raw[start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let end = start + offset + 1;
                    return serde_json::from_str(&raw[start..end]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

/// A prompt-addressed cache of raw judge responses, so an offline run is fully
/// deterministic and a live run never re-pays for an identical prompt.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JudgeCache {
    entries: HashMap<String, String>,
}

impl JudgeCache {
    /// A stable content key for a prompt (FNV-1a, version-independent — unlike
    /// `DefaultHasher`, so a persisted cache stays valid across builds).
    #[must_use]
    pub fn key(prompt: &str) -> String {
        format!("{:016x}", fnv1a(prompt.as_bytes()))
    }

    /// Record a raw response under its prompt.
    pub fn insert(&mut self, prompt: &str, response: impl Into<String>) {
        self.entries.insert(Self::key(prompt), response.into());
    }

    /// The cached raw response for a prompt, if any.
    #[must_use]
    pub fn get(&self, prompt: &str) -> Option<&str> {
        self.entries.get(&Self::key(prompt)).map(String::as_str)
    }
}

/// The judge orchestrator: holds the judge model id and a response cache.
#[derive(Debug, Clone)]
pub struct Judge {
    pub judge_model: String,
    pub cache: JudgeCache,
}

impl Judge {
    /// A judge backed only by its cache (offline-deterministic).
    #[must_use]
    pub fn new(judge_model: impl Into<String>, cache: JudgeCache) -> Self {
        Self {
            judge_model: judge_model.into(),
            cache,
        }
    }

    /// Score one solution from the cache. Blind by construction (the prompt
    /// carries no arm identity). Returns `None` on a cache miss or an unparseable
    /// cached response.
    #[must_use]
    pub fn score_offline(&self, input: &JudgeInput) -> Option<JudgeBlock> {
        let prompt = judge_prompt(input);
        let raw = self.cache.get(&prompt)?;
        parse_judge_block(raw, &self.judge_model, true)
    }

    /// Score one solution with a live judge model, caching the response so a
    /// repeat is free and offline-reproducible. A cache hit short-circuits the
    /// model call. Opportunistic per the validation-evidence policy: the offline
    /// path is the accepted bar; this is run only when a judge model is available.
    ///
    /// # Errors
    /// Returns [`JudgeError`] if the provider cannot be reached/streamed or the
    /// reply does not parse into the four scores.
    pub async fn score_live(
        &mut self,
        provider: &Arc<dyn ModelProvider>,
        model: &str,
        input: &JudgeInput<'_>,
    ) -> Result<JudgeBlock, JudgeError> {
        if let Some(block) = self.score_offline(input) {
            return Ok(block);
        }
        let prompt = judge_prompt(input);
        let raw = collect_completion(provider, model, &prompt).await?;
        self.cache.insert(&prompt, raw.clone());
        parse_judge_block(&raw, &self.judge_model, true).ok_or(JudgeError::Unparseable)
    }

    /// Offline ranking self-test: score the authored [`RANKING_FIXTURES`] from the
    /// cache and require each `better` to outscore its `worse`. A cache that omits
    /// a fixture is untrustworthy — the judge has not been shown to rank, so its
    /// other scores are not trusted. No model runs; the same cache always yields
    /// the same verdict, so it is the offline CI gate.
    #[must_use]
    pub fn ranking_selftest_offline(&self) -> RankingTrust {
        ranking_verdict(RANKING_FIXTURES, |input| self.score_offline(input))
    }

    /// Live ranking self-test: judge the authored fixtures with a real model
    /// (caching each response), then take the offline verdict over the now-warm
    /// cache. Opportunistic per the validation-evidence policy — the offline
    /// gate-logic test is the accepted CI bar; this runs only when a judge model
    /// is available.
    ///
    /// # Errors
    /// Returns [`JudgeError`] if the provider cannot be reached/streamed or a
    /// fixture reply does not parse.
    pub async fn ranking_selftest_live(
        &mut self,
        provider: &Arc<dyn ModelProvider>,
        model: &str,
    ) -> Result<RankingTrust, JudgeError> {
        for fx in RANKING_FIXTURES {
            for diff in [fx.better, fx.worse] {
                self.score_live(
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
        Ok(self.ranking_selftest_offline())
    }

    /// Score one solution offline, but only if the judge first passes its ranking
    /// self-test — the "prove the instrument before spending on it" gate applied to
    /// the judge: an untrustworthy judge refuses to score rather than emit a
    /// believed-but-wrong number.
    ///
    /// # Errors
    /// Returns [`JudgeError::Untrustworthy`] (naming the failed fixture) when the
    /// ranking self-test fails.
    pub fn score_offline_gated(
        &self,
        input: &JudgeInput,
    ) -> Result<Option<JudgeBlock>, JudgeError> {
        match self.ranking_selftest_offline() {
            RankingTrust::Trustworthy => Ok(self.score_offline(input)),
            RankingTrust::Untrustworthy(why) => Err(JudgeError::Untrustworthy(why)),
        }
    }
}

/// A pair of authored solutions to the same task: `better` is, by construction,
/// higher quality than `worse`. Original to this repository (clean-room) — these
/// are not lifted from any external benchmark.
#[derive(Debug, Clone, Copy)]
pub struct RankingFixture {
    /// The quality aspect the pair exercises (named in a failure message).
    pub label: &'static str,
    /// The higher-quality solution.
    pub better: &'static str,
    /// The lower-quality solution to the same task.
    pub worse: &'static str,
}

/// Authored ranking fixtures: a judge worth trusting must score each `better`
/// strictly above its `worse`. Each `worse` variant degrades one dimension the
/// rubric scores — cryptic naming, a blind index that panics on empty input, and
/// a hand-rolled type for a one-line job.
pub const RANKING_FIXTURES: &[RankingFixture] = &[
    RankingFixture {
        label: "readability",
        better: "+fn average(values: &[f64]) -> f64 {\n+    let sum: f64 = values.iter().sum();\n+    sum / values.len() as f64\n+}\n",
        worse: "+fn a(v:&[f64])->f64{let mut s=0.0;let mut i=0;while i<v.len(){s+=v[i];i+=1;}s/v.len() as f64}\n",
    },
    RankingFixture {
        label: "bug_resistance",
        better: "+fn first_word(s: &str) -> Option<&str> {\n+    s.split_whitespace().next()\n+}\n",
        worse: "+fn first_word(s: &str) -> &str {\n+    s.split(' ').collect::<Vec<_>>()[0]\n+}\n",
    },
    RankingFixture {
        label: "abstraction_fit",
        better: "+let unique: HashSet<_> = items.iter().collect();\n",
        worse: "+struct UniqueTracker { seen: Vec<Item>, index: HashMap<Item, usize> }\n+impl UniqueTracker {\n+    fn new() -> Self { Self { seen: vec![], index: HashMap::new() } }\n+    fn add(&mut self, it: Item) { if !self.index.contains_key(&it) { self.index.insert(it.clone(), self.seen.len()); self.seen.push(it); } }\n+}\n",
    },
];

/// The outcome of a ranking self-test: either the judge ranked every `better`
/// above its `worse`, or it failed on a named fixture and is not trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RankingTrust {
    /// The judge ranked every authored `better` strictly above its `worse`.
    Trustworthy,
    /// The judge failed on a fixture (named with the reason); scores not trusted.
    Untrustworthy(String),
}

impl RankingTrust {
    /// Whether the judge passed (ranked every fixture correctly).
    #[must_use]
    pub fn is_trustworthy(&self) -> bool {
        matches!(self, RankingTrust::Trustworthy)
    }
}

/// Score every fixture's pair with `score` and require `overall(better) >
/// overall(worse)` strictly. A parse miss (an unseen fixture) or a non-strict
/// ordering means the judge cannot reliably tell better from worse — untrusted.
fn ranking_verdict<F>(fixtures: &[RankingFixture], mut score: F) -> RankingTrust
where
    F: FnMut(&JudgeInput) -> Option<JudgeBlock>,
{
    for fx in fixtures {
        let better = score(&JudgeInput {
            diff: fx.better,
            trajectory: None,
        });
        let worse = score(&JudgeInput {
            diff: fx.worse,
            trajectory: None,
        });
        match (better, worse) {
            (Some(b), Some(w)) if b.overall > w.overall => {}
            (Some(b), Some(w)) => {
                return RankingTrust::Untrustworthy(format!(
                    "{}: better scored {:.2} but worse scored {:.2} (expected strictly higher)",
                    fx.label, b.overall, w.overall
                ));
            }
            _ => {
                return RankingTrust::Untrustworthy(format!(
                    "{}: a sample produced no score",
                    fx.label
                ));
            }
        }
    }
    RankingTrust::Trustworthy
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

/// Cohen's kappa for two raters' integer labels over the same items: agreement
/// corrected for chance. `1.0` is perfect agreement, `0.0` is chance-level.
/// Returns `1.0` for empty or single-category inputs that trivially agree.
///
/// # Panics
/// Never; callers pass equal-length label lists. Unequal lengths are truncated to
/// the shorter, so the function is total.
#[must_use]
pub fn cohens_kappa(a: &[u8], b: &[u8]) -> f64 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 1.0;
    }
    let pairs = a.iter().zip(b.iter()).take(n);
    let agree = pairs.clone().filter(|(x, y)| x == y).count();
    let po = agree as f64 / n as f64;

    // Expected agreement: sum over categories of the product of each rater's
    // marginal frequency for that category.
    let mut count_a: HashMap<u8, usize> = HashMap::new();
    let mut count_b: HashMap<u8, usize> = HashMap::new();
    for (x, y) in pairs {
        *count_a.entry(*x).or_default() += 1;
        *count_b.entry(*y).or_default() += 1;
    }
    let pe: f64 = count_a
        .keys()
        .chain(count_b.keys())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .map(|k| {
            let fa = *count_a.get(k).unwrap_or(&0) as f64 / n as f64;
            let fb = *count_b.get(k).unwrap_or(&0) as f64 / n as f64;
            fa * fb
        })
        .sum();

    if (1.0 - pe).abs() < f64::EPSILON {
        // Both raters used a single category and agreed everywhere.
        return 1.0;
    }
    (po - pe) / (1.0 - pe)
}

/// 64-bit FNV-1a. Stable across builds/platforms (unlike the std hasher), so a
/// persisted cache key never silently changes.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RESPONSE: &str =
        "{\"readability\":4,\"idiomaticity\":5,\"abstraction_fit\":4,\"bug_resistance\":3}";

    #[test]
    fn parses_a_clean_json_reply() {
        let block = parse_judge_block(SAMPLE_RESPONSE, "judge-x", true).expect("parse");
        assert_eq!(block.readability, 4);
        assert_eq!(block.bug_resistance, 3);
        assert!((block.overall - 4.0).abs() < f64::EPSILON);
        assert!(block.blinded);
    }

    #[test]
    fn parses_a_reply_wrapped_in_prose() {
        let raw = format!("Here is my assessment:\n{SAMPLE_RESPONSE}\nThanks!");
        assert!(parse_judge_block(&raw, "judge-x", true).is_some());
    }

    #[test]
    fn rejects_out_of_range_scores() {
        let raw =
            "{\"readability\":9,\"idiomaticity\":5,\"abstraction_fit\":4,\"bug_resistance\":3}";
        assert!(parse_judge_block(raw, "judge-x", true).is_none());
    }

    #[test]
    fn scores_from_cache_without_a_model() {
        let input = JudgeInput {
            diff: "+ fixed the off-by-one",
            trajectory: None,
        };
        let mut cache = JudgeCache::default();
        cache.insert(&judge_prompt(&input), SAMPLE_RESPONSE);
        let judge = Judge::new("judge-x", cache);
        let block = judge
            .score_offline(&input)
            .expect("cache hit scores offline");
        assert_eq!(block.judge_model, "judge-x");
        assert!(block.blinded);
        // A different input misses the cache deterministically.
        let other = JudgeInput {
            diff: "+ something else entirely",
            trajectory: None,
        };
        assert!(judge.score_offline(&other).is_none());
    }

    #[test]
    fn blinding_randomizes_order_but_maps_back() {
        // Find two seeds that produce opposite orders, then confirm the verdict
        // resolves to the same arm regardless of presentation.
        let unswapped = (0..100).find(|s| !blind("A", "B", *s).swapped).unwrap();
        let swapped = (0..100).find(|s| blind("A", "B", *s).swapped).unwrap();

        let p_unswapped = blind("alpha", "beta", unswapped);
        assert_eq!(p_unswapped.solution_1, "alpha");
        // Judge prefers position 1 → resolves to the first arm.
        assert_eq!(resolve_preference(&p_unswapped, 1), Some(Preferred::First));

        let p_swapped = blind("alpha", "beta", swapped);
        assert_eq!(p_swapped.solution_1, "beta");
        // Judge prefers position 1 → but that is the *second* arm here.
        assert_eq!(resolve_preference(&p_swapped, 1), Some(Preferred::Second));
        assert_eq!(resolve_preference(&p_swapped, 3), None);
    }

    #[test]
    fn preference_reply_parses() {
        assert_eq!(parse_preference("{\"preferred\":2}"), Some(2));
        assert_eq!(parse_preference("I prefer {\"preferred\":1}."), Some(1));
        assert_eq!(parse_preference("{\"preferred\":7}"), None);
    }

    #[test]
    fn kappa_is_one_for_perfect_agreement_and_low_for_chance() {
        let human = [5u8, 4, 3, 5, 2, 4];
        assert!((cohens_kappa(&human, &human) - 1.0).abs() < f64::EPSILON);

        // A judge that disagrees on several items scores well below 1.
        let judge = [5u8, 5, 5, 5, 5, 5];
        let k = cohens_kappa(&human, &judge);
        assert!(k < 1.0, "disagreement lowers kappa (got {k})");
        assert!((-1.0..=1.0).contains(&k));
    }

    #[test]
    fn calibration_reports_agreement_on_a_labelled_sample() {
        // A small original human-labelled sample, paired with the judge's labels
        // (as if read from cache). Calibration reports their agreement.
        let human = [4u8, 5, 2, 3, 5];
        let judge = [4u8, 5, 3, 3, 5];
        let kappa = cohens_kappa(&human, &judge);
        // Four of five agree; kappa is high but not perfect.
        assert!(kappa > 0.5, "substantial agreement expected (got {kappa})");
        assert!(kappa < 1.0);
    }

    #[test]
    fn fnv_key_is_stable_and_distinguishes_prompts() {
        assert_eq!(JudgeCache::key("hello"), JudgeCache::key("hello"));
        assert_ne!(JudgeCache::key("hello"), JudgeCache::key("world"));
    }

    /// A uniform judge response with all four dimensions at `s`.
    fn resp(s: u8) -> String {
        format!(
            "{{\"readability\":{s},\"idiomaticity\":{s},\"abstraction_fit\":{s},\"bug_resistance\":{s}}}"
        )
    }

    /// A cache that scores every fixture's `better` at `better` and `worse` at
    /// `worse`, so the ranking self-test can be exercised with no model.
    fn ranked_cache(better: u8, worse: u8) -> JudgeCache {
        let mut cache = JudgeCache::default();
        for fx in RANKING_FIXTURES {
            cache.insert(
                &judge_prompt(&JudgeInput {
                    diff: fx.better,
                    trajectory: None,
                }),
                resp(better),
            );
            cache.insert(
                &judge_prompt(&JudgeInput {
                    diff: fx.worse,
                    trajectory: None,
                }),
                resp(worse),
            );
        }
        cache
    }

    #[test]
    fn ranking_selftest_passes_when_better_outscores_worse() {
        let judge = Judge::new("j", ranked_cache(5, 2));
        assert_eq!(judge.ranking_selftest_offline(), RankingTrust::Trustworthy);
        assert!(judge.ranking_selftest_offline().is_trustworthy());
    }

    #[test]
    fn ranking_selftest_fails_on_an_inverted_judge() {
        // worse scores higher than better — the judge cannot tell them apart.
        let judge = Judge::new("j", ranked_cache(2, 5));
        match judge.ranking_selftest_offline() {
            RankingTrust::Untrustworthy(why) => {
                assert!(
                    why.contains("readability"),
                    "names the failing fixture: {why}"
                );
            }
            RankingTrust::Trustworthy => panic!("an inverted judge must be untrustworthy"),
        }
    }

    #[test]
    fn ranking_selftest_fails_when_a_fixture_is_unseen() {
        // An empty cache cannot demonstrate that the judge ranks anything.
        let judge = Judge::new("j", JudgeCache::default());
        assert!(!judge.ranking_selftest_offline().is_trustworthy());
    }

    #[test]
    fn gated_scoring_refuses_an_untrustworthy_judge() {
        let input = JudgeInput {
            diff: "+ a real change",
            trajectory: None,
        };
        // A trustworthy judge that also has the task diff cached returns a score.
        let mut cache = ranked_cache(5, 2);
        cache.insert(&judge_prompt(&input), resp(4));
        let good = Judge::new("j", cache);
        assert!(good
            .score_offline_gated(&input)
            .expect("a trustworthy judge scores")
            .is_some());

        // An inverted judge refuses with an error and never returns a score.
        let bad = Judge::new("j", ranked_cache(2, 5));
        assert!(matches!(
            bad.score_offline_gated(&input),
            Err(JudgeError::Untrustworthy(_))
        ));
    }
}
