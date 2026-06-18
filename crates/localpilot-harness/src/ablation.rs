//! Ablation arms, per-feature attribution, and the composite score.
//!
//! The eval answers *which harness features move the score, by how much*. That
//! needs three pieces this module owns, all deterministic and model-free so they
//! are offline-testable:
//! - the **arm matrix** — `baseline` (raw loop), `full` (every feature on), and
//!   one arm per feature turned off, with the model pinned across arms so a delta
//!   grades the harness, not the model;
//! - **attribution** — each feature is mapped to the process signal it should
//!   move, and a feature that is on but does not move its signal is flagged;
//! - the **composite** — correctness gates first, then passers rank by quality +
//!   process + regression-safety; a failed trajectory keeps its process score in a
//!   separate bucket. Speed is a reported guardrail, never part of the score.

use serde::{Deserialize, Serialize};

use crate::scorecard::Scorecard;

/// The harness features an ablation toggles. `full` has all on; `baseline` all
/// off; each per-feature arm flips exactly one off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureToggles {
    pub retrieval: bool,
    pub code_intelligence: bool,
    pub tool_budget: bool,
    pub check_before_launch: bool,
    pub tool_pull_discovery: bool,
}

impl FeatureToggles {
    /// Every feature enabled (the `full` arm).
    #[must_use]
    pub fn all_on() -> Self {
        Self {
            retrieval: true,
            code_intelligence: true,
            tool_budget: true,
            check_before_launch: true,
            tool_pull_discovery: true,
        }
    }

    /// Every feature disabled (the `baseline` raw loop).
    #[must_use]
    pub fn all_off() -> Self {
        Self {
            retrieval: false,
            code_intelligence: false,
            tool_budget: false,
            check_before_launch: false,
            tool_pull_discovery: false,
        }
    }
}

/// One arm of the ablation: a name plus the feature configuration it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AblationArm {
    pub name: String,
    pub features: FeatureToggles,
}

/// The features in attribution order, paired with the process signal each is
/// expected to move and an accessor for that signal off a scorecard.
const FEATURES: &[(&str, &str)] = &[
    ("retrieval", "retrieval_used"),
    ("code_intelligence", "reproduce_before_fix"),
    ("tool_budget", "tool_calls"),
    ("check_before_launch", "reproduce_before_fix"),
    ("tool_pull_discovery", "redundant_calls"),
];

/// The standard ablation matrix: `baseline`, `full`, and one `no-<feature>` arm
/// per feature (that feature off, the rest on), so each arm isolates one feature.
#[must_use]
pub fn ablation_matrix() -> Vec<AblationArm> {
    let mut arms = vec![
        AblationArm {
            name: "baseline".to_string(),
            features: FeatureToggles::all_off(),
        },
        AblationArm {
            name: "full".to_string(),
            features: FeatureToggles::all_on(),
        },
    ];
    for (feature, _) in FEATURES {
        let mut features = FeatureToggles::all_on();
        set_feature(&mut features, feature, false);
        arms.push(AblationArm {
            name: format!("no-{feature}"),
            features,
        });
    }
    arms
}

/// Flip one feature by name on a toggle set. Unknown names are a no-op.
fn set_feature(features: &mut FeatureToggles, name: &str, value: bool) {
    match name {
        "retrieval" => features.retrieval = value,
        "code_intelligence" => features.code_intelligence = value,
        "tool_budget" => features.tool_budget = value,
        "check_before_launch" => features.check_before_launch = value,
        "tool_pull_discovery" => features.tool_pull_discovery = value,
        _ => {}
    }
}

/// The process signal a feature is expected to move (e.g. `retrieval` →
/// `retrieval_used`). `None` for an unknown feature.
#[must_use]
pub fn feature_signal(feature: &str) -> Option<&'static str> {
    FEATURES
        .iter()
        .find(|(name, _)| *name == feature)
        .map(|(_, signal)| *signal)
}

/// One attribution finding: a feature, the signal it should move, the signal's
/// value with the feature on (`full`) vs off (the `no-<feature>` arm), and
/// whether removing the feature actually changed the signal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttributionRow {
    pub feature: String,
    pub signal: String,
    pub full_value: f64,
    pub ablated_value: f64,
    /// Whether the signal moved when the feature was removed. A feature that is on
    /// but does not move its signal is **inert** (`moved == false`) — a smell the
    /// report surfaces.
    pub moved: bool,
}

/// The numeric value of a named process signal on a scorecard, for attribution.
#[must_use]
pub fn signal_value(card: &Scorecard, signal: &str) -> f64 {
    let p = &card.process;
    match signal {
        "retrieval_used" => f64::from(u8::from(p.retrieval_used)),
        "reproduce_before_fix" => f64::from(u8::from(p.reproduce_before_fix)),
        "test_before_done" => f64::from(u8::from(p.test_before_done)),
        "tool_calls" => f64::from(p.tool_calls),
        "redundant_calls" => f64::from(p.redundant_calls),
        "retrieval_count" => f64::from(p.retrieval_count),
        _ => 0.0,
    }
}

/// Attribute each feature's effect by comparing the `full` arm's scorecard to the
/// matching `no-<feature>` arm's scorecard: did the feature move the signal it is
/// supposed to? A row whose `moved` is false flags an inert feature.
#[must_use]
pub fn attribute(full: &Scorecard, ablated: &[(String, Scorecard)]) -> Vec<AttributionRow> {
    let mut rows = Vec::new();
    for (feature, signal) in FEATURES {
        let Some((_, card)) = ablated
            .iter()
            .find(|(arm, _)| arm == &format!("no-{feature}"))
        else {
            continue;
        };
        let full_value = signal_value(full, signal);
        let ablated_value = signal_value(card, signal);
        rows.push(AttributionRow {
            feature: (*feature).to_string(),
            signal: (*signal).to_string(),
            full_value,
            ablated_value,
            moved: (full_value - ablated_value).abs() > f64::EPSILON,
        });
    }
    rows
}

/// The composite outcome for one scorecard: correctness gates. A passing task gets
/// a `0.0..=1.0` composite (quality + process + regression-safety); a failing task
/// is bucketed separately but keeps its process-only score, so a failed trajectory
/// is still comparable to other failures without ever outranking a pass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CompositeOutcome {
    /// The task passed; carries the composite score.
    Passed(f64),
    /// The task failed; carries the process-only score (kept separate).
    Failed(f64),
}

impl CompositeOutcome {
    /// A total-order key: every pass outranks every failure, ties broken by score.
    #[must_use]
    pub fn rank_key(&self) -> (u8, f64) {
        match self {
            Self::Passed(score) => (1, *score),
            Self::Failed(score) => (0, *score),
        }
    }
}

/// The quality sub-score in `0.0..=1.0`: gate cleanliness, diff minimality vs gold,
/// and tests-added credit.
fn quality_component(card: &Scorecard) -> f64 {
    let q = &card.quality;
    let clean = (f64::from(u8::from(q.format_clean))
        + f64::from(u8::from(q.lint_clean))
        + f64::from(u8::from(q.typecheck_clean)))
        / 3.0;
    // A vs-gold ratio near 1 is minimal; larger is bloated. No gold → neutral.
    let minimal = match q.vs_gold_ratio {
        Some(ratio) => (1.0 / ratio.max(1.0)).clamp(0.0, 1.0),
        None => 0.5,
    };
    let tests = if q.tests_added { 1.0 } else { 0.5 };
    (clean + minimal + tests) / 3.0
}

/// The process sub-score in `0.0..=1.0`: the disciplined-behaviour signals,
/// discounted by the redundant-call rate.
fn process_component(card: &Scorecard) -> f64 {
    let p = &card.process;
    let good = (f64::from(u8::from(p.reproduce_before_fix))
        + f64::from(u8::from(p.test_before_done))
        + f64::from(u8::from(p.retrieval_used)))
        / 3.0;
    let redundant_rate = if p.tool_calls == 0 {
        0.0
    } else {
        f64::from(p.redundant_calls) / f64::from(p.tool_calls)
    };
    (good * (1.0 - redundant_rate)).clamp(0.0, 1.0)
}

/// Compute the composite outcome for a scorecard. Correctness is the gate; speed
/// is never part of the score (it is a reported guardrail).
#[must_use]
pub fn composite_score(card: &Scorecard) -> CompositeOutcome {
    let process = process_component(card);
    if !card.results.passed {
        return CompositeOutcome::Failed(process);
    }
    let quality = quality_component(card);
    let regression = f64::from(u8::from(card.results.regression_safe));
    let composite = (quality + process + regression) / 3.0;
    CompositeOutcome::Passed(composite)
}

/// Rank scorecards best-first: every passer (by composite, descending) before any
/// failure (by process score, descending). Returns indices into `cards`.
#[must_use]
pub fn rank(cards: &[Scorecard]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..cards.len()).collect();
    order.sort_by(|&a, &b| {
        let ka = composite_score(&cards[a]).rank_key();
        let kb = composite_score(&cards[b]).rank_key();
        kb.partial_cmp(&ka).unwrap_or(std::cmp::Ordering::Equal)
    });
    order
}

/// Mean and sample standard deviation of N-seed values, for variance bars. A
/// single value has zero deviation.
#[must_use]
pub fn mean_std(values: &[f64]) -> (f64, f64) {
    let n = values.len();
    if n == 0 {
        return (0.0, 0.0);
    }
    let mean = values.iter().sum::<f64>() / n as f64;
    if n == 1 {
        return (mean, 0.0);
    }
    let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0);
    (mean, variance.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scorecard::{
        ProcessBlock, QualityBlock, ResultsBlock, SpeedBlock, SCORECARD_SCHEMA,
    };

    fn card(passed: bool, tool_calls: u32, redundant: u32, retrieval: bool) -> Scorecard {
        Scorecard {
            schema: SCORECARD_SCHEMA,
            task: "t".to_string(),
            arm: "full".to_string(),
            model: "fake".to_string(),
            results: ResultsBlock {
                passed,
                regression_safe: true,
                partial_credit: if passed { 1.0 } else { 0.0 },
                tests_total: 1,
                tests_passed: u32::from(passed),
            },
            quality: QualityBlock {
                diff_added: 4,
                diff_removed: 1,
                diff_files: 1,
                vs_gold_ratio: Some(1.0),
                format_clean: true,
                lint_clean: true,
                typecheck_clean: true,
                complexity_delta: Some(0),
                tests_added: false,
            },
            process: ProcessBlock {
                tool_calls,
                redundant_calls: redundant,
                reproduce_before_fix: true,
                test_before_done: true,
                retrieval_used: retrieval,
                retrieval_count: u32::from(retrieval),
                exit_reason: "Done".to_string(),
                recovered_after_failure: false,
                discipline: None,
            },
            speed: SpeedBlock {
                wall_ms: 100,
                input_tokens: 1,
                output_tokens: 1,
            },
            judge: None,
        }
    }

    #[test]
    fn matrix_has_baseline_full_and_one_arm_per_feature() {
        let matrix = ablation_matrix();
        assert!(matrix.iter().any(|a| a.name == "baseline"));
        assert!(matrix.iter().any(|a| a.name == "full"));
        assert_eq!(matrix.len(), 2 + FEATURES.len());
        let baseline = matrix.iter().find(|a| a.name == "baseline").unwrap();
        assert_eq!(baseline.features, FeatureToggles::all_off());
        // The `no-retrieval` arm has retrieval off but everything else on.
        let no_retrieval = matrix.iter().find(|a| a.name == "no-retrieval").unwrap();
        assert!(!no_retrieval.features.retrieval);
        assert!(no_retrieval.features.code_intelligence);
    }

    #[test]
    fn correctness_gates_the_composite() {
        let pass = composite_score(&card(true, 3, 0, true));
        let fail = composite_score(&card(false, 3, 0, true));
        assert!(matches!(pass, CompositeOutcome::Passed(_)));
        assert!(matches!(fail, CompositeOutcome::Failed(_)));
        // A failed trajectory never outranks a pass, even with a strong process.
        assert!(pass.rank_key() > fail.rank_key());
    }

    #[test]
    fn ranking_puts_passers_first_then_by_score() {
        let cards = vec![
            card(false, 3, 0, true), // fail
            card(true, 8, 4, true),  // pass, lots of redundancy
            card(true, 3, 0, true),  // pass, clean
        ];
        let order = rank(&cards);
        // The clean pass ranks first, the redundant pass second, the failure last.
        assert_eq!(order[0], 2);
        assert_eq!(order[1], 1);
        assert_eq!(order[2], 0);
    }

    #[test]
    fn redundant_calls_lower_the_process_score() {
        let clean = process_component(&card(true, 4, 0, true));
        let noisy = process_component(&card(true, 4, 2, true));
        assert!(noisy < clean);
    }

    #[test]
    fn attribution_flags_an_inert_feature() {
        // `full` used retrieval; the `no-retrieval` arm also (wrongly) shows
        // retrieval used → the feature is inert (its signal did not move).
        let full = card(true, 3, 0, true);
        let ablated = vec![
            ("no-retrieval".to_string(), card(true, 3, 0, true)), // signal unchanged → inert
            ("no-tool_pull_discovery".to_string(), card(true, 3, 2, true)), // redundant moved
        ];
        let rows = attribute(&full, &ablated);
        let retrieval = rows.iter().find(|r| r.feature == "retrieval").unwrap();
        assert!(!retrieval.moved, "retrieval signal did not move → inert");
        let pull = rows
            .iter()
            .find(|r| r.feature == "tool_pull_discovery")
            .unwrap();
        assert!(
            pull.moved,
            "redundant_calls moved when the broker was removed"
        );
    }

    #[test]
    fn feature_signal_maps_known_features() {
        assert_eq!(feature_signal("retrieval"), Some("retrieval_used"));
        assert_eq!(feature_signal("unknown"), None);
    }

    #[test]
    fn mean_std_reports_variance() {
        let (mean, std) = mean_std(&[2.0, 4.0, 6.0]);
        assert!((mean - 4.0).abs() < f64::EPSILON);
        assert!(std > 1.9 && std < 2.1); // sample stddev of {2,4,6} = 2.0
        assert_eq!(mean_std(&[5.0]), (5.0, 0.0));
    }
}
