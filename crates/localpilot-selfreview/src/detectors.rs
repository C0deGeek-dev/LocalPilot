//! Read-only repo-health detectors.
//!
//! Each detector inspects tracked files and emits [`Finding`]s. The whole pass is
//! read-only: it walks (honouring ignore files), reads UTF-8 text, and never
//! writes, deletes, or executes anything. Detectors are independent and
//! individually testable; [`scan`] runs them in one bounded walk.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::Path;

use regex::Regex;

use crate::cleanup::{self, DuplicateAggregate, TraitImplAggregate};
use crate::finding::{Finding, FindingKind, Severity, Span};

/// Files larger than this are skipped (a health scan needn't read blobs).
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Cap on the walk's directory depth, so a deep tree stays bounded.
const MAX_DIR_DEPTH: usize = 32;

/// Run every detector over `root` in one read-only walk. Returns the findings and
/// the number of files read.
///
/// `include_cleanup` turns on the whole-repo teardown-sweep detectors (the
/// cleanup-audit categories); it is off by default so the always-on `self-review`
/// surface is unchanged unless a caller opts in.
#[must_use]
pub fn scan(
    root: &Path,
    include_missing_tests: bool,
    include_cleanup: bool,
) -> (Vec<Finding>, usize) {
    let mut findings = Vec::new();
    let mut scanned = 0_usize;
    let mut adr = AdrAggregate::default();
    let mut duplicates = DuplicateAggregate::default();
    let mut traits = TraitImplAggregate::default();
    let mut saw_cargo_manifest = false;

    // `hidden(false)` is needed so a real hidden project dir (`.github/` workflows)
    // is in scope; `.git` itself is VCS internals, not project content, so it is
    // excluded explicitly regardless — this also skips `.git/modules/*` submodule
    // mirrors, since the whole `.git` subtree is never descended into.
    let walker = ignore::WalkBuilder::new(root)
        .max_depth(Some(MAX_DIR_DEPTH))
        .hidden(false)
        .filter_entry(|entry| entry.file_name() != OsStr::new(".git"))
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

        // A crate's own `tests/` integration files exist to *contain* marker
        // strings as fixture data (e.g. a literal TODO-style code comment
        // written to a temp file, to prove a detector finds it) — scanning
        // them for real markers means the detector finds its own tests.
        if !in_tests_dir(&rel) {
            findings.extend(todo_markers(&rel, &text));
        }
        if is_markdown(path) {
            findings.extend(doc_links(root, path, &rel, &text));
            findings.extend(plan_health(&rel, &text));
        }
        if include_missing_tests && is_rust_source(path) {
            findings.extend(missing_tests(&rel, &text));
        }
        if include_cleanup {
            saw_cargo_manifest |= is_cargo_manifest(path);
            findings.extend(cleanup::legacy_file(&rel));
            if is_rust_source(path) {
                findings.extend(cleanup::dead_code_allows(&rel, &text));
                findings.extend(cleanup::redundant_access(&rel, &text));
                duplicates.observe(&rel, &text);
                traits.observe(&rel, &text);
            }
        }
        adr.observe(&rel, &text);
    }

    findings.extend(adr.stale_findings());
    if include_cleanup {
        findings.extend(duplicates.findings());
        findings.extend(traits.findings());
        if saw_cargo_manifest {
            findings.extend(cleanup::tool_pointers());
        }
    }
    (findings, scanned)
}

/// A path component named `tests` — Rust's convention for a crate's integration
/// test files, which exist to hold marker-like fixture strings, not real markers.
fn in_tests_dir(rel: &str) -> bool {
    rel.split('/').any(|component| component == "tests")
}

/// The line a real, file-scope `#[cfg(test)]` attribute starts on, if any. A
/// fixture *describing* the attribute as a string (e.g. a test that writes
/// `"...\n#[cfg(test)]\nmod tests {...}"` to a temp file) never appears as its
/// own exact line once trimmed, so this only finds the attribute in real source.
/// By this codebase's convention the test module is the last item in the file,
/// so treating everything from there to EOF as test code is enough — no brace
/// tracking, which a fixture string's own `{`/`}` characters would throw off.
fn test_module_start(text: &str) -> Option<usize> {
    text.lines().position(|line| line.trim() == "#[cfg(test)]")
}

/// `TODO`/`FIXME`/`XXX`/`HACK` markers. `FIXME` is treated as the most serious.
fn todo_markers(rel: &str, text: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    let test_start = test_module_start(text);
    for (index, line) in text.lines().enumerate() {
        if test_start.is_some_and(|start| index >= start) {
            continue;
        }
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

/// The first recognised marker keyword sitting at a real comment position on a
/// line — immediately after a `//`/`///`/`//!` or `#` comment leader — and its
/// severity. Requiring comment position (rather than a bare word match anywhere
/// on the line) is what tells a real leftover TODO comment apart from prose
/// *about* the markers (a changelog entry, a doc comment, a spec listing what
/// the detector looks for) and from the detector's own match table, none of
/// which put the word right after a comment leader.
fn marker_in(line: &str) -> Option<(&'static str, Severity)> {
    const MARKERS: [(&str, Severity); 4] = [
        ("FIXME", Severity::Medium),
        ("TODO", Severity::Low),
        ("XXX", Severity::Low),
        ("HACK", Severity::Low),
    ];
    for leader in comment_leader_starts(line) {
        let after = line[leader..].trim_start_matches(['/', '#', '!', ' ', '\t']);
        for (marker, severity) in MARKERS {
            if let Some(rest) = after.strip_prefix(marker) {
                let next_ok = rest.as_bytes().first().is_none_or(|b| !is_word_byte(*b));
                if next_ok {
                    return Some((marker, severity));
                }
            }
        }
    }
    None
}

/// Byte offsets in `line` where a line-comment leader (`//` or `#`) begins.
fn comment_leader_starts(line: &str) -> Vec<usize> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'/') {
            out.push(i);
            i += 2;
        } else if bytes[i] == b'#' {
            out.push(i);
            i += 1;
        } else {
            i += 1;
        }
    }
    out
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
    let mut in_fence = false;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue; // a fenced code sample isn't a real doc link.
        }
        for capture in pattern.captures_iter(line) {
            let Some(whole) = capture.get(0) else {
                continue; // group 0 always matches in practice; skip defensively.
            };
            if inside_inline_code(line, whole.start()) {
                continue; // e.g. `` `[text](url)` `` explaining link syntax, not a real link.
            }
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

/// Whether byte offset `pos` on `line` falls inside a span quoted by `delim` —
/// an odd number of `delim` bytes precede it, so `pos` sits in the segment the
/// pair quotes rather than the surrounding prose.
fn inside_quoted_span(line: &str, pos: usize, delim: u8) -> bool {
    line.as_bytes()[..pos.min(line.len())]
        .iter()
        .filter(|&&b| b == delim)
        .count()
        % 2
        == 1
}

/// Whether byte offset `pos` on `line` falls inside a single-backtick inline
/// code span — e.g. `` `[text](url)` `` explaining link syntax, not a real link.
fn inside_inline_code(line: &str, pos: usize) -> bool {
    inside_quoted_span(line, pos, b'`')
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
        let lower = trimmed.to_ascii_lowercase();
        if let Some(pos) = lower.find("pending sign-off") {
            // A quoted mention (e.g. a spec citing the phrase as an example of
            // what this detector looks for) is documentation, not a real
            // unresolved row — `lower` is byte-for-byte the same length as
            // `trimmed` (ASCII-only case change), so `pos` applies to both.
            if !inside_quoted_span(trimmed, pos, b'"') {
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

/// Whether a file is a `Cargo.toml` (the marker that tool-owned categories apply).
fn is_cargo_manifest(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("Cargo.toml"))
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
    fn doc_links_skip_inline_code_and_fenced_examples() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("doc.md");
        // Both examples name a target ("url", "nope.md") that resolves to nothing
        // — they'd be flagged as broken if read as real links, but one is quoted
        // as inline code (explaining link syntax) and the other sits in a fenced
        // sample; neither is a real link a reader would follow.
        let text = "Example: `[text](url)` is the Markdown link syntax.\n\n\
             ```\n[also](nope.md)\n```\n";
        std::fs::write(&file, text).unwrap();
        let findings = doc_links(dir.path(), &file, "doc.md", text);
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn todo_markers_requires_comment_position_not_bare_prose() {
        // A backtick-quoted mention in prose (a changelog/spec entry describing
        // the markers) has no comment leader before it — not a real leftover.
        let prose = "- leftover `TODO`/`FIXME` markers, a decision index lagging...\n";
        assert!(todo_markers("CHANGELOG.md", prose).is_empty(), "{prose}");
        // The detector's own match-table literal: no comment leader on the line.
        let table = "        (\"TODO\", Severity::Low),\n";
        assert!(todo_markers("detectors.rs", table).is_empty(), "{table}");
        // A `#`-led comment (shell/markdown heading style) still counts.
        let hashed = "# TODO: revisit this section\n";
        assert_eq!(todo_markers("notes.md", hashed).len(), 1);
    }

    #[test]
    fn todo_markers_skips_its_own_cfg_test_module() {
        let text = "// TODO: real leftover\n\
             pub fn f() {}\n\
             \n\
             #[cfg(test)]\n\
             mod tests {\n\
             \x20   fn t() {\n\
             \x20       let s = \"pub fn run() {}\\n// TODO: handle retries\\n\";\n\
             \x20   }\n\
             }\n";
        let findings = todo_markers("src/lib.rs", text);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].span.unwrap().start_line, 1);
    }

    #[test]
    fn scan_skips_git_internals_and_crate_test_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
        std::fs::write(root.join(".git/hooks/sample"), "# TODO: sample hook\n").unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("tests/fixture.rs"), "// TODO: fixture marker\n").unwrap();
        std::fs::write(root.join("real.rs"), "// TODO: a real leftover\n").unwrap();

        let (findings, _scanned) = scan(root, false, false);
        let todos: Vec<_> = findings
            .iter()
            .filter(|f| f.kind == FindingKind::Todo)
            .collect();
        assert_eq!(todos.len(), 1, "{todos:?}");
        assert_eq!(todos[0].path.as_deref(), Some("real.rs"));
    }

    #[test]
    fn plan_health_flags_todo_rows_and_pending_signoff() {
        let text = "| 1 | TODO |\n| 2 | DONE |\nstatus: pending sign-off\n";
        let findings = plan_health("p.md", text);
        assert_eq!(findings.len(), 2);
        assert!(findings.iter().all(|f| f.kind == FindingKind::BrokenPlan));
    }

    #[test]
    fn plan_health_skips_pending_signoff_cited_as_a_quoted_example() {
        // A spec line *naming* the phrase as an example of what this detector
        // looks for, not a real unresolved row.
        let text = "rows (`TODO` status cells, \"pending sign-off\"), broken local doc links\n";
        assert!(plan_health("docs/spec.md", text).is_empty());
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
