//! Findings engine: turn a [`ContextInventory`](crate::ContextInventory) into
//! severity-ranked, advisory findings.
//!
//! The analyzers are pure over the inventory — no I/O, no mutation. They are
//! deliberately conservative: they flag only high-confidence overlaps, so the
//! report stays trusted rather than noisy. Redundancy and conflict detection
//! reuse the store's text-overlap primitives; severities reuse the self-review
//! `Severity` scale. Over-constraint is advisory only — a rule that reads as
//! over-specific may still be load-bearing for a weaker local backend, so the
//! finding recommends review, never removal.

use std::collections::{BTreeMap, BTreeSet};

use localmind_store::{similarity, token_set};
use localpilot_selfreview::Severity;
use serde::Serialize;

use crate::{ContextInventory, InventorySummary};

/// What kind of context-hygiene issue a finding reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextFindingKind {
    /// The same directive appears, near-verbatim, in more than one layer.
    Redundancy,
    /// Two near-identical directives disagree (one negates the other).
    Conflict,
    /// A layer is large enough that it likely over-constrains — a candidate for
    /// right-sizing. Advisory: the guidance may still be load-bearing.
    OverConstraint,
    /// A skill's body is large enough to be a split candidate.
    OversizedSkill,
    /// Total authored context exceeds the configured token budget.
    TokenBudget,
}

/// One advisory finding: what, how severe, which layers, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextFinding {
    /// The kind of issue.
    pub kind: ContextFindingKind,
    /// Severity on the shared self-review scale.
    pub severity: Severity,
    /// The source labels of the layer(s) involved.
    pub layers: Vec<String>,
    /// A one-line human-readable statement of the issue.
    pub message: String,
    /// A short, already-redacted evidence snippet, when one applies.
    pub evidence: Option<String>,
}

/// The full context-hygiene report: the inventory summary plus ranked findings.
/// This is the shape a caller serializes into a machine-readable report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextReport {
    /// Per-layer token weights and totals.
    pub summary: InventorySummary,
    /// Findings, ranked most-severe first.
    pub findings: Vec<ContextFinding>,
}

/// Tunable, advisory thresholds. Defaults are conservative heuristics, not hard
/// limits — every finding is advice, not an error.
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    /// Minimum token-set overlap (0.0–1.0) for two directives to count as the
    /// "same" directive. Higher = fewer, higher-confidence redundancy findings.
    pub redundancy_similarity: f32,
    /// A single instruction layer above this token estimate is flagged as an
    /// over-constraint / right-sizing candidate.
    pub layer_token_budget: usize,
    /// A skill body above this token estimate is flagged as a split candidate.
    pub skill_token_budget: usize,
    /// Total authored context above this token estimate is flagged.
    pub total_token_budget: usize,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            redundancy_similarity: 0.8,
            layer_token_budget: 2_000,
            skill_token_budget: 1_500,
            total_token_budget: 10_000,
        }
    }
}

/// Cap on directives compared per report, to bound the pairwise redundancy scan.
const MAX_DIRECTIVES: usize = 1_000;
/// A directive must carry at least this many substantive tokens (after polarity
/// words are removed) to be compared — below this, matches are trivial noise.
const MIN_DIRECTIVE_TOKENS: usize = 3;

/// Polarity/quantifier words removed before comparing two directives, so that
/// "always use tabs" and "never use tabs" compare as the *same* directive and
/// are then separated by the raw-text negation check into redundancy vs
/// conflict. Without this, the differing polarity word drags overlap below the
/// threshold and a genuine contradiction reads as two unrelated lines.
const POLARITY: [&str; 9] = [
    "always", "never", "not", "dont", "avoid", "no", "without", "must", "should",
];

/// Analyze an inventory into a ranked, advisory report.
#[must_use]
pub fn analyze(inventory: &ContextInventory, thresholds: &Thresholds) -> ContextReport {
    let mut findings = Vec::new();
    findings.extend(cross_layer_findings(inventory, thresholds));
    findings.extend(size_findings(inventory, thresholds));
    findings.sort_by(|a, b| severity_rank(b.severity).cmp(&severity_rank(a.severity)));
    ContextReport {
        summary: inventory.summary(),
        findings,
    }
}

/// One comparable directive: its source layer, its (already-redacted) text, and
/// its substantive token set.
struct Directive {
    source: String,
    text: String,
    tokens: BTreeSet<String>,
}

/// Split a redacted body into comparable directives: non-trivial lines, stripped
/// of list/heading markers.
fn directives_of(source: &str, body: &str) -> Vec<Directive> {
    body.lines()
        .map(|line| {
            line.trim()
                .trim_start_matches(['-', '*', '#', '>', ' ', '\t'])
                .trim()
                .to_string()
        })
        .filter(|line| !line.is_empty())
        .filter(|line| !is_cross_reference(line))
        .filter_map(|text| {
            let tokens = comparison_tokens(&text);
            (tokens.len() >= MIN_DIRECTIVE_TOKENS).then_some(Directive {
                source: source.to_string(),
                text,
                tokens,
            })
        })
        .collect()
}

/// Whether a line is a cross-reference — a `[[wiki-link]]` or a "(see …)"
/// parenthetical — rather than an authored directive. Skills share these link
/// slugs (e.g. `(see [[clean-room-guard]])`) across many files, so counting them
/// as directives floods redundancy findings with noise.
fn is_cross_reference(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.contains("[[") || trimmed.starts_with("(see") || trimmed.starts_with("(See")
}

/// The substantive token set of a directive with polarity/quantifier words
/// removed, so contradictions compare as the same directive (see [`POLARITY`]).
fn comparison_tokens(text: &str) -> BTreeSet<String> {
    token_set(text)
        .into_iter()
        .filter(|token| !POLARITY.contains(&token.as_str()))
        .collect()
}

/// Redundancy + conflict: compare directives across *different* layers.
fn cross_layer_findings(
    inventory: &ContextInventory,
    thresholds: &Thresholds,
) -> Vec<ContextFinding> {
    let mut directives: Vec<Directive> = Vec::new();
    for layer in &inventory.layers {
        for directive in directives_of(&layer.source, &layer.body) {
            directives.push(directive);
            if directives.len() >= MAX_DIRECTIVES {
                break;
            }
        }
        if directives.len() >= MAX_DIRECTIVES {
            break;
        }
    }

    // Conflicts are reported per directive-pair (rare and specific); redundancy
    // is collapsed to one finding per unordered layer-pair (a shared convention
    // restated N times between the same two layers is one issue, not N).
    let mut findings = Vec::new();
    let mut redundant: BTreeMap<(String, String), (usize, String)> = BTreeMap::new();
    for i in 0..directives.len() {
        for j in (i + 1)..directives.len() {
            let (a, b) = (&directives[i], &directives[j]);
            if a.source == b.source {
                continue; // cross-layer only
            }
            if similarity(&a.tokens, &b.tokens) < thresholds.redundancy_similarity {
                continue;
            }
            if negation_differs(&a.text, &b.text) {
                findings.push(ContextFinding {
                    kind: ContextFindingKind::Conflict,
                    severity: Severity::Medium,
                    layers: vec![a.source.clone(), b.source.clone()],
                    message: "Two near-identical directives disagree across layers.".to_string(),
                    evidence: Some(format!("{}  ⇄  {}", a.text, b.text)),
                });
            } else {
                let key = if a.source <= b.source {
                    (a.source.clone(), b.source.clone())
                } else {
                    (b.source.clone(), a.source.clone())
                };
                let entry = redundant.entry(key).or_insert((0, a.text.clone()));
                entry.0 += 1;
            }
        }
    }
    for ((a, b), (count, evidence)) in redundant {
        let message = if count == 1 {
            "The same directive is stated in more than one layer.".to_string()
        } else {
            format!("{count} directives are repeated across these two layers.")
        };
        findings.push(ContextFinding {
            kind: ContextFindingKind::Redundancy,
            severity: Severity::Low,
            layers: vec![a, b],
            message,
            evidence: Some(evidence),
        });
    }
    findings
}

/// Over-constraint, oversized-skill, and total-token-budget findings.
fn size_findings(inventory: &ContextInventory, thresholds: &Thresholds) -> Vec<ContextFinding> {
    use crate::LayerKind;
    let mut findings = Vec::new();

    for layer in &inventory.layers {
        match &layer.kind {
            LayerKind::Skill { name } if layer.tokens > thresholds.skill_token_budget => {
                findings.push(ContextFinding {
                    kind: ContextFindingKind::OversizedSkill,
                    severity: Severity::Low,
                    layers: vec![layer.source.clone()],
                    message: format!(
                        "Skill '{name}' is large ({} tokens) — a split candidate (progressive disclosure).",
                        layer.tokens
                    ),
                    evidence: None,
                });
            }
            LayerKind::Instruction { .. } if layer.tokens > thresholds.layer_token_budget => {
                findings.push(ContextFinding {
                    kind: ContextFindingKind::OverConstraint,
                    severity: Severity::Low,
                    layers: vec![layer.source.clone()],
                    message: format!(
                        "Instruction layer is large ({} tokens) — review for right-sizing; keep guidance a weaker model still needs.",
                        layer.tokens
                    ),
                    evidence: None,
                });
            }
            _ => {}
        }
    }

    let total: usize = inventory.layers.iter().map(|l| l.tokens).sum();
    if total > thresholds.total_token_budget {
        findings.push(ContextFinding {
            kind: ContextFindingKind::TokenBudget,
            severity: Severity::Medium,
            layers: inventory.layers.iter().map(|l| l.source.clone()).collect(),
            message: format!(
                "Total authored context is {total} tokens, over the {} budget.",
                thresholds.total_token_budget
            ),
            evidence: None,
        });
    }

    findings
}

/// Whether exactly one of two directives carries a negation — a cheap, high-
/// precision signal that two otherwise-identical directives disagree.
fn negation_differs(a: &str, b: &str) -> bool {
    has_negation(a) != has_negation(b)
}

fn has_negation(text: &str) -> bool {
    const MARKERS: [&str; 7] = ["never", "not", "don't", "dont", "avoid", "no", "without"];
    // Word-boundary match, not substring: a substring check reports "note",
    // "another", and "notice" as negations ("not") and floods conflict findings
    // with false positives (found by the internal sweep).
    text.to_ascii_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .any(|word| MARKERS.contains(&word))
}

/// Rank for ordering — higher is more severe.
fn severity_rank(severity: Severity) -> u8 {
    match severity {
        Severity::High => 3,
        Severity::Medium => 2,
        Severity::Low => 1,
        Severity::Info => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContextLayer, LayerKind};

    fn instruction_layer(source: &str, body: &str, tokens: usize) -> ContextLayer {
        ContextLayer {
            kind: LayerKind::Instruction {
                kind: "Claude".to_string(),
                scope: "Project".to_string(),
            },
            source: source.to_string(),
            body: body.to_string(),
            tokens,
        }
    }

    #[test]
    fn flags_a_directive_repeated_across_layers() {
        let inv = ContextInventory {
            layers: vec![
                instruction_layer("CLAUDE.md", "Always match the surrounding code style.", 8),
                instruction_layer("AGENTS.md", "Always match the surrounding code style.", 8),
            ],
        };
        let report = analyze(&inv, &Thresholds::default());
        let redundancies: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.kind == ContextFindingKind::Redundancy)
            .collect();
        assert_eq!(redundancies.len(), 1);
        assert_eq!(redundancies[0].layers.len(), 2);
    }

    #[test]
    fn cross_reference_lines_are_not_treated_as_directives() {
        // A `(see [[...]])` cross-reference shared across layers must not become a
        // redundancy finding — the noise the precision pass removes.
        let inv = ContextInventory {
            layers: vec![
                instruction_layer(
                    "CLAUDE.md",
                    "Follow the policy (see [[clean-room-guard]]).",
                    8,
                ),
                instruction_layer(
                    "AGENTS.md",
                    "Follow the policy (see [[clean-room-guard]]).",
                    8,
                ),
            ],
        };
        let report = analyze(&inv, &Thresholds::default());
        assert!(
            report.findings.is_empty(),
            "cross-reference lines must not produce findings"
        );
    }

    #[test]
    fn redundancy_is_collapsed_to_one_finding_per_layer_pair() {
        let body = "Always write tests for new code.\nPrefer small focused functions.";
        let inv = ContextInventory {
            layers: vec![
                instruction_layer("CLAUDE.md", body, 12),
                instruction_layer("AGENTS.md", body, 12),
            ],
        };
        let report = analyze(&inv, &Thresholds::default());
        let redundancies: Vec<_> = report
            .findings
            .iter()
            .filter(|f| f.kind == ContextFindingKind::Redundancy)
            .collect();
        assert_eq!(
            redundancies.len(),
            1,
            "two shared directives across one layer-pair is one finding"
        );
        assert!(redundancies[0].message.contains("2 directives"));
    }

    #[test]
    fn flags_a_contradiction_between_layers() {
        let inv = ContextInventory {
            layers: vec![
                instruction_layer(
                    "CLAUDE.md",
                    "Always use tabs for indentation in this project.",
                    9,
                ),
                instruction_layer(
                    "AGENTS.md",
                    "Never use tabs for indentation in this project.",
                    9,
                ),
            ],
        };
        let report = analyze(&inv, &Thresholds::default());
        assert!(report
            .findings
            .iter()
            .any(|f| f.kind == ContextFindingKind::Conflict));
    }

    #[test]
    fn negation_is_matched_at_word_boundaries_not_as_a_substring() {
        // "note" must not read as the negation "not" — the false-positive the
        // internal sweep surfaced. Two identical lines are redundant, not a conflict.
        let inv = ContextInventory {
            layers: vec![
                instruction_layer("CLAUDE.md", "See the provenance note in the guide.", 8),
                instruction_layer("AGENTS.md", "See the provenance note in the guide.", 8),
            ],
        };
        let report = analyze(&inv, &Thresholds::default());
        assert!(
            report
                .findings
                .iter()
                .all(|f| f.kind != ContextFindingKind::Conflict),
            "identical directives containing 'note' must not read as a conflict"
        );
        assert!(report
            .findings
            .iter()
            .any(|f| f.kind == ContextFindingKind::Redundancy));
    }

    #[test]
    fn a_clean_single_layer_yields_no_findings() {
        let inv = ContextInventory {
            layers: vec![instruction_layer(
                "CLAUDE.md",
                "Prefer small, focused functions.",
                6,
            )],
        };
        let report = analyze(&inv, &Thresholds::default());
        assert!(report.findings.is_empty(), "clean context should be quiet");
    }

    #[test]
    fn flags_an_oversized_skill_and_ranks_budget_first() {
        let inv = ContextInventory {
            layers: vec![
                ContextLayer {
                    kind: LayerKind::Skill {
                        name: "huge".to_string(),
                    },
                    source: ".localpilot/skills/huge".to_string(),
                    body: "big skill".to_string(),
                    tokens: 9_000,
                },
                instruction_layer("CLAUDE.md", "Be concise.", 4_000),
            ],
        };
        let report = analyze(&inv, &Thresholds::default());
        assert!(report
            .findings
            .iter()
            .any(|f| f.kind == ContextFindingKind::OversizedSkill));
        assert!(report
            .findings
            .iter()
            .any(|f| f.kind == ContextFindingKind::TokenBudget));
        // TokenBudget (Medium) must rank above OversizedSkill (Low).
        assert_eq!(report.findings[0].kind, ContextFindingKind::TokenBudget);
    }

    #[test]
    fn report_round_trips_through_json() {
        let inv = ContextInventory {
            layers: vec![instruction_layer("CLAUDE.md", "Prefer clarity.", 5)],
        };
        let report = analyze(&inv, &Thresholds::default());
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"summary\""));
        assert!(json.contains("\"findings\""));
    }
}
