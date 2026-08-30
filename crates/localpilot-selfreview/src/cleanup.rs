//! Whole-repo teardown-sweep detectors (the cleanup-audit categories).
//!
//! These extend the read-only repo scan with the cruft signals a completion-time
//! cleanup sweep wants: dead/abandoned code, duplicate/parallel logic,
//! over-engineering, and redundant data access. They are advisory and conservative
//! by construction — every finding records the hidden-usage channels it weighed,
//! and none reaches high confidence from the absence of local references alone (the
//! safety invariant). Categories already owned by tooling (unused deps, unused
//! imports/vars, advisories) are surfaced as a [`FindingKind::ToolPointer`] that
//! names the authoritative command rather than re-deriving it.
//!
//! Like [`crate::detectors`], everything here reads text and emits findings; it
//! writes, deletes, and executes nothing.

use std::collections::{HashMap, HashSet};

use regex::Regex;

use crate::finding::{Finding, FindingKind, Risk, Severity, Span};

/// The hidden-usage channels the safety invariant weighs before any removal is
/// recommended. A detector records the subset it ruled out so the reader sees the
/// finding was not raised from missing local references alone.
const HIDDEN_USAGE_CHANNELS: [&str; 11] = [
    "reflection",
    "dependency injection",
    "routing",
    "serialization",
    "config activation",
    "db migrations",
    "background jobs",
    "external callers",
    "public API",
    "tests",
    "build/CI",
];

/// Number of consecutive substantive lines that must match across two files for a
/// duplicate-logic finding. Large enough that incidental one-liners do not collide.
const DUP_WINDOW: usize = 6;

/// A file whose name marks it as a backup, abandoned, or deprecated copy. Purely
/// name-based (it never reasons from missing references), so confidence is moderate
/// and a human confirms before deleting.
#[must_use]
pub fn legacy_file(rel: &str) -> Option<Finding> {
    let name = rel.rsplit(['/', '\\']).next().unwrap_or(rel);
    let lower = name.to_ascii_lowercase();
    let abandoned = lower.ends_with('~')
        || [".bak", ".old", ".orig", ".disabled", ".tmp"]
            .iter()
            .any(|s| lower.ends_with(s))
        || [
            "_old.",
            "_backup.",
            "_deprecated.",
            "_copy.",
            "_legacy.",
            ".bak.",
        ]
        .iter()
        .any(|s| lower.contains(s));
    if !abandoned {
        return None;
    }
    Some(
        Finding::new(
            FindingKind::DeadCode,
            Severity::Low,
            0.7,
            format!("'{name}' looks like a backup/abandoned file"),
        )
        .at_path(rel)
        .with_risk(Risk::Low)
        .recommending("remove if superseded, after confirming nothing references it")
        .channels_checked(HIDDEN_USAGE_CHANNELS)
        .owned_by("agent"),
    )
}

/// Explicit `#[allow(dead_code)]` allowances: the compiler already judged the item
/// unused and it was silenced rather than removed. Low confidence — the item may be
/// intentionally kept (a public-ish API, a test helper) — so the channels checked
/// are recorded and the reader decides. Only a real attribute line counts, so a
/// comment or string that merely mentions the lint is not flagged.
#[must_use]
pub fn dead_code_allows(rel: &str, text: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let attr = line.trim_start();
        if (attr.starts_with("#[") || attr.starts_with("#![")) && attr.contains("allow(dead_code") {
            out.push(
                Finding::new(
                    FindingKind::DeadCode,
                    Severity::Low,
                    0.5,
                    format!("dead-code warning silenced: {}", line.trim()),
                )
                .at_path(rel)
                .at_span(Span::line((index + 1) as u64))
                .with_risk(Risk::Low)
                .recommending("remove the item, or the allow, once it is genuinely used")
                .channels_checked(HIDDEN_USAGE_CHANNELS)
                .owned_by("agent"),
            );
        }
    }
    out
}

/// The same data-fetch call repeated within one file — a redundant-access signal
/// (the value is likely fetched once-too-often). Conservative: it dedups on the
/// fetch call signature (so `let a = read(x)` and `let b = read(x)` match), not a
/// semantic equivalent, so confidence is low.
#[must_use]
pub fn redundant_access(rel: &str, text: &str) -> Vec<Finding> {
    let mut first_seen: HashMap<String, u64> = HashMap::new();
    let mut reported: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let Some(signature) = access_signature(line.trim()) else {
            continue;
        };
        if signature.len() < 8 {
            continue; // too trivial to be a meaningful fetch.
        }
        let line_no = (index + 1) as u64;
        if let Some(&first) = first_seen.get(&signature) {
            if reported.insert(signature.clone()) {
                out.push(
                    Finding::new(
                        FindingKind::RedundantDataAccess,
                        Severity::Low,
                        0.4,
                        format!(
                            "identical data access `{signature}` repeated (first at line {first})"
                        ),
                    )
                    .at_path(rel)
                    .at_span(Span::line(line_no))
                    .with_risk(Risk::Low)
                    .recommending("fetch once and reuse the result when the value is stable")
                    .owned_by("agent"),
                );
            }
        } else {
            first_seen.insert(signature, line_no);
        }
    }
    out
}

/// The data-fetch call on a line, from a fetch/read/query needle to its matching
/// close paren, so two reads of the same source dedup even with different
/// assignment targets (`let a = read(x)` and `let b = read(x)`). `None` when the
/// line performs no recognised data access.
fn access_signature(line: &str) -> Option<String> {
    const NEEDLES: [&str; 6] = [
        "read_to_string(",
        "fs::read(",
        "::open(",
        ".query(",
        ".fetch(",
        ".execute(",
    ];
    let (start, open) = NEEDLES
        .iter()
        .filter_map(|needle| {
            line.find(needle)
                .map(|index| (index, index + needle.len() - 1))
        })
        .min_by_key(|(index, _)| *index)?;
    let bytes = line.as_bytes();
    let mut depth = 0_usize;
    for (index, &byte) in bytes.iter().enumerate().skip(open) {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(line[start..=index].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// Pointer findings for the categories tooling already owns, naming the
/// authoritative command instead of re-deriving it. Emitted once per scan when the
/// repo is a Cargo workspace.
#[must_use]
pub fn tool_pointers() -> Vec<Finding> {
    [
        ("cargo machete", "unused dependencies"),
        (
            "cargo clippy --workspace --all-targets",
            "unused imports/variables and dead-code warnings",
        ),
        (
            "cargo deny check",
            "dependency advisories and license/source bans",
        ),
    ]
    .into_iter()
    .map(|(command, owns)| {
        Finding::new(
            FindingKind::ToolPointer,
            Severity::Info,
            0.6,
            format!("{owns} are owned by `{command}` — run and cite it, do not re-derive"),
        )
        .with_risk(Risk::Low)
        .recommending(format!("run `{command}`"))
        .owned_by("agent")
    })
    .collect()
}

/// Cross-file duplicate-logic aggregate: a run of [`DUP_WINDOW`] substantive lines
/// that appears in two different files is flagged once, naming both locations.
#[derive(Default)]
pub struct DuplicateAggregate {
    /// First `(file, start_line)` seen for a window hash.
    first: HashMap<u64, (String, u64)>,
    /// Window hashes already reported, so each duplicate yields one finding.
    reported: HashSet<u64>,
    findings: Vec<Finding>,
}

impl DuplicateAggregate {
    /// Fold one file's substantive line-windows into the aggregate.
    pub fn observe(&mut self, rel: &str, text: &str) {
        let lines: Vec<(u64, &str)> = text
            .lines()
            .enumerate()
            .map(|(index, line)| ((index + 1) as u64, line.trim()))
            .filter(|(_, trimmed)| is_substantive(trimmed))
            .collect();
        if lines.len() < DUP_WINDOW {
            return;
        }
        // Collapse overlapping/adjacent windows in this file into one finding per
        // contiguous duplicated run, so a long shared block is not reported once
        // per sliding window.
        let mut covered_until = 0_u64;
        for window in lines.windows(DUP_WINDOW) {
            let hash = hash_window(window.iter().map(|(_, text)| *text));
            let start_line = window[0].0;
            match self.first.get(&hash) {
                None => {
                    self.first.insert(hash, (rel.to_string(), start_line));
                }
                Some((first_file, first_line)) => {
                    if first_file != rel && start_line > covered_until && self.reported.insert(hash)
                    {
                        covered_until = window[DUP_WINDOW - 1].0;
                        let first_file = first_file.clone();
                        let first_line = *first_line;
                        self.findings.push(
                            Finding::new(
                                FindingKind::DuplicateLogic,
                                Severity::Low,
                                0.5,
                                format!(
                                    "{DUP_WINDOW} consecutive lines duplicate {first_file}:{first_line}"
                                ),
                            )
                            .at_path(rel)
                            .at_span(Span::line(start_line))
                            .with_risk(Risk::Medium)
                            .recommending(format!(
                                "consolidate with {first_file}:{first_line} if they are one behaviour"
                            ))
                            .channels_checked(["tests", "external callers"])
                            .owned_by("agent"),
                        );
                    }
                }
            }
        }
    }

    /// Drain the accumulated duplicate-logic findings.
    #[must_use]
    pub fn findings(self) -> Vec<Finding> {
        self.findings
    }
}

/// Cross-file over-engineering aggregate: a non-public trait with exactly one
/// implementor in the repo is a single-user abstraction worth questioning. Public
/// traits are excluded — an external caller is a legitimate (hidden) second user.
#[derive(Default)]
pub struct TraitImplAggregate {
    /// Non-public trait name -> the `(file, line)` of its definition.
    private_traits: HashMap<String, (String, u64)>,
    /// Trait name -> number of `impl ... for` blocks observed.
    impl_counts: HashMap<String, u32>,
}

impl TraitImplAggregate {
    /// Fold one file's trait definitions and trait impls into the aggregate.
    pub fn observe(&mut self, rel: &str, text: &str) {
        let Ok(trait_def) = Regex::new(r"(?m)^\s*(pub\s+(?:\([^)]*\)\s*)?)?trait\s+([A-Za-z_]\w*)")
        else {
            return;
        };
        let Ok(impl_for) =
            Regex::new(r"\bimpl\b(?:\s*<[^>]*>)?\s+([A-Za-z_][\w:]*)(?:\s*<[^>]*>)?\s+for\b")
        else {
            return;
        };
        for capture in trait_def.captures_iter(text) {
            if capture.get(1).is_some() {
                continue; // a public trait: external callers may implement it.
            }
            let Some(name) = capture.get(2) else {
                continue;
            };
            self.private_traits
                .entry(name.as_str().to_string())
                .or_insert_with(|| (rel.to_string(), line_of(text, name.start())));
        }
        for capture in impl_for.captures_iter(text) {
            let Some(raw) = capture.get(1) else {
                continue;
            };
            let name = raw.as_str().rsplit("::").next().unwrap_or(raw.as_str());
            *self.impl_counts.entry(name.to_string()).or_insert(0) += 1;
        }
    }

    /// Emit a finding per private trait with exactly one implementor.
    #[must_use]
    pub fn findings(self) -> Vec<Finding> {
        let mut out = Vec::new();
        for (name, (file, line)) in &self.private_traits {
            if self.impl_counts.get(name).copied() == Some(1) {
                out.push(
                    Finding::new(
                        FindingKind::OverEngineering,
                        Severity::Info,
                        0.35,
                        format!("private trait `{name}` has a single implementor"),
                    )
                    .at_path(file)
                    .at_span(Span::line(*line))
                    .with_risk(Risk::Low)
                    .recommending("inline the single implementor unless a second is imminent")
                    .channels_checked(["external callers", "public API"])
                    .owned_by("agent"),
                );
            }
        }
        // Deterministic order regardless of the map's iteration order.
        out.sort_by(|a, b| a.path.cmp(&b.path).then(a.evidence.cmp(&b.evidence)));
        out
    }
}

/// Whether a trimmed line is substantive enough to anchor duplicate detection:
/// long enough, not a comment or attribute, and carrying real tokens.
fn is_substantive(trimmed: &str) -> bool {
    trimmed.len() >= 12
        && !trimmed.starts_with("//")
        && !trimmed.starts_with('#')
        && trimmed.chars().any(|c| c.is_alphanumeric())
}

/// A stable hash of a window's lines (deterministic within a scan).
fn hash_window<'a>(lines: impl Iterator<Item = &'a str>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for line in lines {
        line.hash(&mut hasher);
        0xff_u8.hash(&mut hasher); // separator so line boundaries matter.
    }
    hasher.finish()
}

/// The 1-based line a byte offset falls on.
fn line_of(text: &str, byte_pos: usize) -> u64 {
    (text[..byte_pos].bytes().filter(|&b| b == b'\n').count() + 1) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_file_flags_backups_and_abstains_on_real_files() {
        assert!(legacy_file("src/worker.rs.bak").is_some());
        assert!(legacy_file("src/worker_old.rs").is_some());
        assert!(legacy_file("notes.txt~").is_some());
        assert!(legacy_file("src/worker.rs").is_none());
        assert!(legacy_file("Cargo.toml").is_none());
    }

    #[test]
    fn legacy_file_records_channels_and_is_not_high_confidence() {
        let finding = legacy_file("a.rs.old").expect("flagged");
        assert_eq!(finding.kind, FindingKind::DeadCode);
        assert!(
            finding.confidence < 0.8,
            "name-based, never high confidence"
        );
        assert!(!finding.hidden_usage_checked.is_empty());
        assert!(finding.recommendation.is_some());
    }

    #[test]
    fn dead_code_allows_flags_silenced_items() {
        let findings = dead_code_allows("a.rs", "#[allow(dead_code)]\npub fn f() {}\n");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, FindingKind::DeadCode);
        // Matches an allow that also silences other lints.
        let combined = dead_code_allows("a.rs", "#![allow(dead_code, unused)]\n");
        assert_eq!(combined.len(), 1);
        // Abstains when there is no allowance.
        assert!(dead_code_allows("a.rs", "pub fn f() {}\n").is_empty());
    }

    #[test]
    fn redundant_access_flags_a_repeated_read_once() {
        let text = "let a = std::fs::read_to_string(path)?;\n\
                    let b = std::fs::read_to_string(path)?;\n\
                    let c = std::fs::read_to_string(path)?;\n";
        let findings = redundant_access("a.rs", text);
        assert_eq!(
            findings.len(),
            1,
            "one finding per repeated line: {findings:?}"
        );
        assert_eq!(findings[0].kind, FindingKind::RedundantDataAccess);
        // A single access is not redundant.
        assert!(redundant_access("a.rs", "let a = std::fs::read_to_string(path)?;\n").is_empty());
        // A non-access repeated line is ignored.
        assert!(redundant_access(
            "a.rs",
            "let total = a + b + c + d;\nlet total = a + b + c + d;\n"
        )
        .is_empty());
    }

    #[test]
    fn duplicate_aggregate_flags_only_cross_file_blocks() {
        let block = "let alpha = compute_alpha(input);\n\
                     let beta = compute_beta(alpha, input);\n\
                     let gamma = compute_gamma(beta, alpha);\n\
                     let delta = compute_delta(gamma, beta);\n\
                     let epsilon = combine(delta, gamma);\n\
                     return Ok(epsilon.finalize());\n";
        // Same block, two files -> one finding.
        let mut agg = DuplicateAggregate::default();
        agg.observe("a.rs", block);
        agg.observe("b.rs", block);
        let findings = agg.findings();
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].kind, FindingKind::DuplicateLogic);
        assert_eq!(findings[0].risk, Risk::Medium);

        // The same block repeated inside one file is not a cross-file duplicate.
        let mut intra = DuplicateAggregate::default();
        intra.observe("a.rs", &format!("{block}\n{block}"));
        assert!(intra.findings().is_empty());
    }

    #[test]
    fn trait_aggregate_flags_single_impl_private_trait_only() {
        let mut agg = TraitImplAggregate::default();
        agg.observe(
            "a.rs",
            "trait Single {}\nstruct One;\nimpl Single for One {}\n",
        );
        let findings = agg.findings();
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].kind, FindingKind::OverEngineering);

        // A public trait is excluded (external callers may implement it).
        let mut public = TraitImplAggregate::default();
        public.observe(
            "a.rs",
            "pub trait Open {}\nstruct One;\nimpl Open for One {}\n",
        );
        assert!(public.findings().is_empty());

        // A trait with two implementors is not a single-user abstraction.
        let mut multi = TraitImplAggregate::default();
        multi.observe(
            "a.rs",
            "trait Multi {}\nimpl Multi for One {}\nimpl Multi for Two {}\n",
        );
        assert!(multi.findings().is_empty());
    }

    #[test]
    fn tool_pointers_name_the_owning_commands() {
        let findings = tool_pointers();
        assert_eq!(findings.len(), 3);
        assert!(findings.iter().all(|f| f.kind == FindingKind::ToolPointer));
        let blob = findings
            .iter()
            .map(|f| f.evidence.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(blob.contains("cargo machete"));
        assert!(blob.contains("cargo clippy"));
        assert!(blob.contains("cargo deny"));
    }
}
