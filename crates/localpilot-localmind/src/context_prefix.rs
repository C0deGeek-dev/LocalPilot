//! Deterministic, offline per-chunk context prefixes — and the gated, opt-in
//! model-enrichment tier.
//!
//! Prefixing each chunk with a short description of where it sits in its
//! document lifts recall when chunk boundaries split a file mid-thought. The
//! default here is fully synthetic and offline: the file path plus a one-line
//! gist taken from YAML front matter (`title` / `description`) or the file's
//! leading non-empty line. No model, no network — the same file always yields
//! the same prefix.
//!
//! A richer *model-written* prefix is a separate tier that would send file
//! content off-machine. It is therefore opt-in: unreachable unless an explicit
//! flag is set **and** an enricher is wired, and every use records an audit row.
//! With the flag off the synthetic prefix is produced instead, so the
//! deterministic, local path is the contract.

use crate::ingest::IngestError;

/// Hard cap on a stored prefix, so a pathological front matter or leading line
/// cannot bloat the index.
const PREFIX_MAX_CHARS: usize = 200;
/// Cap on the gist drawn from front matter / leading lines.
const GIST_MAX_CHARS: usize = 140;

/// The deterministic, offline prefix for a file's chunks: the path plus a gist
/// from front matter or the leading content line. Stable for stable input.
pub(crate) fn synthetic_context_prefix(display_path: &str, file_text: &str) -> String {
    let mut prefix = format!("File {display_path}");
    if let Some(gist) = front_matter_gist(file_text).or_else(|| leading_line_gist(file_text)) {
        prefix.push_str(": ");
        prefix.push_str(&gist);
    }
    prefix.push('.');
    truncate_chars(&prefix, PREFIX_MAX_CHARS)
}

/// Whether the opt-in model-enrichment tier may run. Off by default; the field
/// is fed from project config so the tier stays unreachable unless the user
/// turns it on.
pub(crate) struct PrefixEnrichmentPolicy {
    pub enabled: bool,
}

/// A model-backed prefix writer. The default ingest path wires `None`, so the
/// off-machine tier is never constructed; tests provide a stub.
pub(crate) trait PrefixEnricher {
    fn enrich(
        &self,
        display_path: &str,
        synthetic: &str,
        file_text: &str,
    ) -> Result<String, IngestError>;
}

/// One audit row recorded when the off-machine enrichment tier runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrefixAudit {
    pub path: String,
    pub tier: String,
}

/// Resolve the context prefix for a file. Without the opt-in flag — or without a
/// wired enricher — returns the deterministic synthetic prefix and never
/// consults the enricher, so the off-machine tier is unreachable and writes no
/// audit row. When enabled *and* an enricher is present, returns its prefix and
/// appends an egress audit row; an enricher error or empty reply falls back to
/// the synthetic prefix.
pub(crate) fn resolve_context_prefix(
    policy: &PrefixEnrichmentPolicy,
    enricher: Option<&dyn PrefixEnricher>,
    display_path: &str,
    file_text: &str,
    audit: &mut Vec<PrefixAudit>,
) -> String {
    let synthetic = synthetic_context_prefix(display_path, file_text);
    if !policy.enabled {
        return synthetic;
    }
    let Some(enricher) = enricher else {
        return synthetic;
    };
    match enricher.enrich(display_path, &synthetic, file_text) {
        Ok(enriched) if !enriched.trim().is_empty() => {
            audit.push(PrefixAudit {
                path: display_path.to_string(),
                tier: "model".to_string(),
            });
            truncate_chars(enriched.trim(), PREFIX_MAX_CHARS)
        }
        _ => synthetic,
    }
}

/// Title/description from leading YAML front matter (`---` … `---`), joined with
/// an em dash. `None` when there is no front matter or neither key is present.
fn front_matter_gist(file_text: &str) -> Option<String> {
    let mut lines = file_text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return None;
    }
    let mut title = None;
    let mut description = None;
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            let value = value.trim().trim_matches(['"', '\'']).trim();
            if value.is_empty() {
                continue;
            }
            match key.trim().to_ascii_lowercase().as_str() {
                "title" => title = Some(value.to_string()),
                "description" => description = Some(value.to_string()),
                _ => {}
            }
        }
    }
    let gist = match (title, description) {
        (Some(title), Some(description)) => format!("{title} — {description}"),
        (Some(title), None) => title,
        (None, Some(description)) => description,
        (None, None) => return None,
    };
    Some(truncate_chars(gist.trim(), GIST_MAX_CHARS))
}

/// The first meaningful content line: the leading non-empty line that is not a
/// front-matter fence, with a Markdown heading's `#` markers stripped. `None`
/// for an empty or fence-only file.
fn leading_line_gist(file_text: &str) -> Option<String> {
    for line in file_text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "---" {
            continue;
        }
        let cleaned = trimmed.trim_start_matches('#').trim();
        if cleaned.is_empty() {
            continue;
        }
        return Some(truncate_chars(cleaned, GIST_MAX_CHARS));
    }
    None
}

/// Char-safe truncation: never splits a multi-byte char and never panics.
fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::cell::Cell;

    #[test]
    fn front_matter_title_and_description_drive_the_prefix() {
        let text = "---\ntitle: Auth Flow\ndescription: token refresh rules\n---\n\nbody\n";
        let prefix = synthetic_context_prefix("docs/auth.md", text);
        assert!(prefix.starts_with("File docs/auth.md: "), "{prefix}");
        assert!(prefix.contains("Auth Flow"), "{prefix}");
        assert!(prefix.contains("token refresh rules"), "{prefix}");
    }

    #[test]
    fn leading_line_is_used_without_front_matter() {
        let text = "# Parser internals\n\nThe parser walks tokens.\n";
        let prefix = synthetic_context_prefix("src/parser.rs", text);
        assert_eq!(prefix, "File src/parser.rs: Parser internals.");
    }

    #[test]
    fn bare_path_prefix_when_no_gist_is_available() {
        let prefix = synthetic_context_prefix("data/empty.txt", "\n\n");
        assert_eq!(prefix, "File data/empty.txt.");
    }

    #[test]
    fn the_prefix_is_deterministic() {
        let text = "# Heading\nbody\n";
        assert_eq!(
            synthetic_context_prefix("a.md", text),
            synthetic_context_prefix("a.md", text)
        );
    }

    /// Records whether it was called, so a disabled policy can be proven to never
    /// reach the off-machine tier.
    struct SpyEnricher<'a> {
        called: &'a Cell<bool>,
    }

    impl PrefixEnricher for SpyEnricher<'_> {
        fn enrich(
            &self,
            _display_path: &str,
            _synthetic: &str,
            _file_text: &str,
        ) -> Result<String, IngestError> {
            self.called.set(true);
            Ok("model-written context".to_string())
        }
    }

    #[test]
    fn disabled_policy_never_reaches_the_enricher_and_writes_no_audit() {
        let called = Cell::new(false);
        let enricher = SpyEnricher { called: &called };
        let mut audit = Vec::new();
        let policy = PrefixEnrichmentPolicy { enabled: false };

        let prefix = resolve_context_prefix(
            &policy,
            Some(&enricher),
            "src/lib.rs",
            "# Lib\nbody\n",
            &mut audit,
        );

        assert!(!called.get(), "the off-machine tier must be unreachable");
        assert!(audit.is_empty(), "no egress means no audit row");
        assert_eq!(prefix, "File src/lib.rs: Lib.");
    }

    #[test]
    fn enabled_policy_with_an_enricher_enriches_and_audits() {
        let called = Cell::new(false);
        let enricher = SpyEnricher { called: &called };
        let mut audit = Vec::new();
        let policy = PrefixEnrichmentPolicy { enabled: true };

        let prefix = resolve_context_prefix(
            &policy,
            Some(&enricher),
            "src/lib.rs",
            "# Lib\nbody\n",
            &mut audit,
        );

        assert!(called.get());
        assert_eq!(prefix, "model-written context");
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].path, "src/lib.rs");
        assert_eq!(audit[0].tier, "model");
    }

    #[test]
    fn enabled_policy_without_an_enricher_stays_synthetic() {
        let mut audit = Vec::new();
        let policy = PrefixEnrichmentPolicy { enabled: true };
        let prefix =
            resolve_context_prefix(&policy, None, "src/lib.rs", "# Lib\nbody\n", &mut audit);
        assert_eq!(prefix, "File src/lib.rs: Lib.");
        assert!(audit.is_empty());
    }
}
