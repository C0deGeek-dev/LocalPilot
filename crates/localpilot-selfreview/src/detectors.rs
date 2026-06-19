//! Read-only repo-health detectors.
//!
//! Each detector inspects tracked files and emits [`Finding`]s. The whole pass is
//! read-only: it walks (honouring ignore files), reads UTF-8 text, and never
//! writes, deletes, or executes anything. Detectors are independent and
//! individually testable; [`scan`] runs them in one bounded walk.

use std::collections::BTreeMap;
use std::path::Path;

use regex::Regex;

use crate::finding::{Finding, FindingKind, Severity, Span};

/// Files larger than this are skipped (a health scan needn't read blobs).
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Cap on the walk's directory depth, so a deep tree stays bounded.
const MAX_DIR_DEPTH: usize = 32;

/// Run every detector over `root` in one read-only walk. Returns the findings and
/// the number of files read.
#[must_use]
pub fn scan(root: &Path, include_missing_tests: bool) -> (Vec<Finding>, usize) {
    let mut findings = Vec::new();
    let mut scanned = 0_usize;
    let mut adr = AdrAggregate::default();

    let walker = ignore::WalkBuilder::new(root)
        .max_depth(Some(MAX_DIR_DEPTH))
        .hidden(false)
        .build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if entry.metadata().map(|m| m.len()).unwrap_or(0) > MAX_FILE_BYTES {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue; // binary or unreadable: a health scan skips it.
        };
        scanned += 1;
        let rel = relative_display(root, path);

        findings.extend(todo_markers(&rel, &text));
        if is_markdown(path) {
            findings.extend(doc_links(root, path, &rel, &text));
            findings.extend(plan_health(&rel, &text));
        }
        if include_missing_tests && is_rust_source(path) {
            findings.extend(missing_tests(&rel, &text));
        }
        adr.observe(&rel, &text);
    }

    findings.extend(adr.stale_findings());
    (findings, scanned)
}

/// `TODO`/`FIXME`/`XXX`/`HACK` markers. `FIXME` is treated as the most serious.
fn todo_markers(rel: &str, text: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let Some((marker, severity)) = marker_in(line) else {
            continue;
        };
        let line_no = (index + 1) as u64;
        out.push(
            Finding::new(
                FindingKind::Todo,
                severity,
                0.9,
                format!("{marker}: {}", line.trim()),
            )
            .at_path(rel)
            .at_span(Span::line(line_no)),
        );
    }
    out
}

/// The first recognised marker keyword on a line and its severity. Matched as a
/// whole word so `todo_list` or `fixmestate` do not trip it.
fn marker_in(line: &str) -> Option<(&'static str, Severity)> {
    for (marker, severity) in [
        ("FIXME", Severity::Medium),
        ("TODO", Severity::Low),
        ("XXX", Severity::Low),
        ("HACK", Severity::Low),
    ] {
        if contains_word(line, marker) {
            return Some((marker, severity));
        }
    }
    None
}

/// Whether `word` appears in `line` bounded by non-alphanumeric/underscore
/// characters (case-sensitive, since these markers are upper-case by convention).
fn contains_word(line: &str, word: &str) -> bool {
    let bytes = line.as_bytes();
    let mut from = 0;
    while let Some(rel) = line[from..].find(word) {
        let start = from + rel;
        let end = start + word.len();
        let before_ok = start == 0 || !is_word_byte(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_word_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Markdown links to local files that do not exist (broken relative links) — a
/// doc-drift signal. Skips `http(s)`, anchors, and mailto.
fn doc_links(root: &Path, file: &Path, rel: &str, text: &str) -> Vec<Finding> {
    // [text](target) — target is captured up to a space (title) or close paren.
    let Ok(pattern) = Regex::new(r"\[[^\]]*\]\(([^)\s]+)") else {
        return Vec::new();
    };
    let base = file.parent().unwrap_or(root);
    let mut out = Vec::new();
    for (index, line) in text.lines().enumerate() {
        for capture in pattern.captures_iter(line) {
            let target = &capture[1];
            if is_external_link(target) {
                continue;
            }
            let local = target.split('#').next().unwrap_or(target);
            if local.is_empty() {
                continue; // pure in-page anchor
            }
            let resolved = base.join(local);
            if !resolved.exists() {
                out.push(
                    Finding::new(
                        FindingKind::DocDrift,
                        Severity::Medium,
                        0.85,
                        format!("broken link to '{local}'"),
                    )
                    .at_path(rel)
                    .at_span(Span::line((index + 1) as u64))
                    .owned_by("docs"),
                );
            }
        }
    }
    out
}

fn is_external_link(target: &str) -> bool {
    let lower = target.to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || lower.starts_with('#')
}

/// Plan/tracking-document health: a status cell of `TODO` in a table row, or
/// "pending sign-off" prose, signals an unresolved tracking row.
fn plan_health(rel: &str, text: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        let line_no = (index + 1) as u64;
        if trimmed.starts_with('|') && contains_word(trimmed, "TODO") {
            out.push(
                Finding::new(
                    FindingKind::BrokenPlan,
                    Severity::Low,
                    0.5,
                    format!("tracking row still TODO: {trimmed}"),
                )
                .at_path(rel)
                .at_span(Span::line(line_no)),
            );
        }
        if trimmed.to_ascii_lowercase().contains("pending sign-off") {
            out.push(
                Finding::new(
                    FindingKind::BrokenPlan,
                    Severity::Medium,
                    0.6,
                    "unresolved 'pending sign-off'".to_string(),
                )
                .at_path(rel)
                .at_span(Span::line(line_no)),
            );
        }
    }
    out
}

/// Heuristic missing-test signal: a Rust source file that exposes public API but
/// carries no in-file test marker. Low confidence (it cannot see sibling test
/// crates), so ranking keeps it well below concrete findings. Skips the usual
/// entry/aggregator files.
fn missing_tests(rel: &str, text: &str) -> Vec<Finding> {
    let name = rel.rsplit(['/', '\\']).next().unwrap_or(rel);
    if matches!(name, "lib.rs" | "main.rs" | "mod.rs" | "build.rs") {
        return Vec::new();
    }
    let exposes_api = text.contains("pub fn ") || text.contains("pub struct ");
    let has_tests = text.contains("#[cfg(test)]") || text.contains("#[test]");
    if exposes_api && !has_tests {
        return vec![Finding::new(
            FindingKind::MissingTest,
            Severity::Low,
            0.3,
            "public API with no co-located tests".to_string(),
        )
        .at_path(rel)
        .owned_by("agent")];
    }
    Vec::new()
}

/// Cross-file aggregate that detects a decision **index** (registry) lagging the
/// actual decision **log**.
#[derive(Default)]
struct AdrAggregate {
    /// Highest decision number seen in a decision-log file, with its file.
    log_max: BTreeMap<String, (u32, String)>,
    /// Highest decision number an index/registry file claims, with its file.
    index_max: BTreeMap<String, (u32, String)>,
}

impl AdrAggregate {
    fn observe(&mut self, rel: &str, text: &str) {
        let lower = rel.to_ascii_lowercase();
        let is_index = lower.contains("registry");
        let is_log = lower.contains("decision") || lower.contains("decisions");
        if !is_index && !is_log {
            return;
        }
        // Both `ADR-####` and `D-LM-####` share the trailing number; track each
        // series by its prefix so a registry is compared against its own log.
        for (series, number) in decision_ids(text) {
            let target = if is_index {
                &mut self.index_max
            } else {
                &mut self.log_max
            };
            let entry = target.entry(series).or_insert((0, rel.to_string()));
            if number > entry.0 {
                *entry = (number, rel.to_string());
            }
        }
    }

    fn stale_findings(&self) -> Vec<Finding> {
        let mut out = Vec::new();
        for (series, (log_n, log_file)) in &self.log_max {
            let claimed = self.index_max.get(series);
            if let Some((index_n, index_file)) = claimed {
                if log_n > index_n {
                    out.push(
                        Finding::new(
                            FindingKind::StaleAdr,
                            Severity::Medium,
                            0.75,
                            format!(
                                "{index_file} lags the decision log: latest {series}-{log_n:04} in {log_file}, but the index tops out at {series}-{index_n:04}"
                            ),
                        )
                        .at_path(index_file)
                        .owned_by("tech-lead"),
                    );
                }
            }
        }
        out
    }
}

/// Decision identifiers in `text` as `(series, number)` pairs, e.g. `ADR-0034` →
/// `("ADR", 34)` and `D-LM-0014` → `("D-LM", 14)`.
fn decision_ids(text: &str) -> Vec<(String, u32)> {
    let Ok(pattern) = Regex::new(r"\b(ADR|D-LM)-(\d{3,4})\b") else {
        return Vec::new();
    };
    pattern
        .captures_iter(text)
        .filter_map(|capture| {
            let series = capture.get(1)?.as_str().to_string();
            let number = capture.get(2)?.as_str().parse::<u32>().ok()?;
            Some((series, number))
        })
        .collect()
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md"))
}

fn is_rust_source(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e == "rs")
}

/// Display a path project-relative with forward slashes, for stable, portable
/// finding locations.
fn relative_display(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn todo_markers_match_words_and_rank_fixme_higher() {
        let findings = todo_markers("a.rs", "// TODO: x\nlet todo_list = 1;\n// FIXME: y\n");
        // Two markers: TODO (low) and FIXME (medium). `todo_list` is not a marker.
        assert_eq!(findings.len(), 2);
        let fixme = findings
            .iter()
            .find(|f| f.evidence.contains("FIXME"))
            .unwrap();
        assert_eq!(fixme.severity, Severity::Medium);
        assert_eq!(fixme.span.unwrap().start_line, 3);
    }

    #[test]
    fn doc_links_flag_only_broken_local_links() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("there.md"), "hi").unwrap();
        let file = dir.path().join("doc.md");
        let text = "[ok](./there.md) [bad](./gone.md) [ext](https://example.com) [anchor](#top)";
        std::fs::write(&file, text).unwrap();
        let findings = doc_links(dir.path(), &file, "doc.md", text);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].evidence.contains("gone.md"));
        assert_eq!(findings[0].kind, FindingKind::DocDrift);
    }

    #[test]
    fn plan_health_flags_todo_rows_and_pending_signoff() {
        let text = "| 1 | TODO |\n| 2 | DONE |\nstatus: pending sign-off\n";
        let findings = plan_health("p.md", text);
        assert_eq!(findings.len(), 2);
        assert!(findings.iter().all(|f| f.kind == FindingKind::BrokenPlan));
    }

    #[test]
    fn missing_tests_flags_untested_api_but_not_tested_or_entry_files() {
        assert_eq!(missing_tests("src/x.rs", "pub fn f() {}\n").len(), 1);
        assert!(missing_tests("src/x.rs", "pub fn f() {}\n#[test]\nfn t() {}").is_empty());
        // Entry/aggregator files are exempt.
        assert!(missing_tests("src/lib.rs", "pub fn f() {}\n").is_empty());
        // No public API → nothing to test from here.
        assert!(missing_tests("src/x.rs", "fn private() {}\n").is_empty());
    }

    #[test]
    fn stale_adr_flags_a_registry_that_lags_the_log() {
        let mut adr = AdrAggregate::default();
        adr.observe("docs/decisions.md", "## ADR-0007\n## ADR-0008\n");
        adr.observe("REGISTRY.md", "latest ADR-0007\n");
        let findings = adr.stale_findings();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, FindingKind::StaleAdr);
        assert_eq!(findings[0].path.as_deref(), Some("REGISTRY.md"));
        assert!(findings[0].evidence.contains("ADR-0008"));
    }

    #[test]
    fn stale_adr_silent_when_registry_is_current() {
        let mut adr = AdrAggregate::default();
        adr.observe("docs/decisions.md", "## ADR-0008\n");
        adr.observe("REGISTRY.md", "latest ADR-0008\n");
        assert!(adr.stale_findings().is_empty());
    }
}
