//! Session-friction findings: the second findings source (the audit-prompt half
//! of the harness-friction observe-channel).
//!
//! While a model works a real coding task it can audit its own harness friction
//! ("used tool X, got Y, wanted Z; this is missing; this broke") and emit a
//! structured block. [`parse_friction_findings`] normalises that block into the
//! same [`Finding`] shape the repo scan uses, so both sources rank together. This
//! is read-only: it parses text into findings and emits nothing else. Auto-
//! instrumenting the harness to capture per-tool-call friction is a deferred
//! follow-up; this is the minimal audit-prompt source.

use serde::Deserialize;

use crate::finding::{Finding, FindingKind, Severity};

/// The audit prompt a host runs to elicit a friction-findings block. Original to
/// this repository. It asks for a strict JSON array so the output parses
/// deterministically; anything else degrades to no findings rather than guesses.
pub const FRICTION_AUDIT_PROMPT: &str = "\
You just worked a real task with this harness. Audit the harness itself, not the \
task. Report only concrete friction you actually hit: a tool that was missing, a \
tool that returned the wrong thing, a step that broke, or guidance that misled \
you. Do not invent problems.\n\n\
Reply with ONLY a JSON array (no prose, no code fence). Each element:\n\
  {\"evidence\": \"<one sentence: used X, got Y, wanted Z>\",\n\
   \"severity\": \"low|medium|high\",   // optional, default low\n\
   \"confidence\": 0.0-1.0,             // optional, default 0.5\n\
   \"path\": \"<relevant file, if any>\" // optional\n\
  }\n\
If you hit no friction, reply with an empty array: []";

/// One raw friction entry as a model emits it. All but `evidence` are optional.
#[derive(Debug, Deserialize)]
struct RawFriction {
    evidence: String,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    confidence: Option<f32>,
    #[serde(default)]
    path: Option<String>,
}

/// Parse a model's friction-findings block into [`Finding`]s of kind
/// [`FindingKind::Friction`]. Tolerant by design: a fenced block is unwrapped, an
/// entry without evidence is dropped, and malformed or empty input yields no
/// findings (never an error) — so a noisy or absent audit degrades safely.
#[must_use]
pub fn parse_friction_findings(block: &str) -> Vec<Finding> {
    let json = extract_json_array(block);
    let Ok(raws) = serde_json::from_str::<Vec<RawFriction>>(&json) else {
        return Vec::new();
    };
    raws.into_iter()
        .filter_map(|raw| {
            let evidence = raw.evidence.trim();
            if evidence.is_empty() {
                return None;
            }
            let severity = parse_severity(raw.severity.as_deref());
            let confidence = raw.confidence.unwrap_or(0.5);
            let mut finding = Finding::new(
                FindingKind::Friction,
                severity,
                confidence,
                evidence.to_string(),
            )
            .owned_by("agent");
            if let Some(path) = raw.path.filter(|p| !p.trim().is_empty()) {
                finding = finding.at_path(path);
            }
            Some(finding)
        })
        .collect()
}

/// Pull the JSON array out of a block that may be wrapped in a ```json fence or
/// surrounded by stray prose: take from the first `[` to the last `]`. Returns an
/// empty array literal when no brackets are present.
fn extract_json_array(block: &str) -> String {
    let start = block.find('[');
    let end = block.rfind(']');
    match (start, end) {
        (Some(s), Some(e)) if e >= s => block[s..=e].to_string(),
        _ => "[]".to_string(),
    }
}

fn parse_severity(raw: Option<&str>) -> Severity {
    match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("high") => Severity::High,
        Some("medium") => Severity::Medium,
        Some("info") => Severity::Info,
        _ => Severity::Low,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_block() {
        let block = r#"[
            {"evidence":"used X, got Y, wanted Z","severity":"high","confidence":0.8,"path":"a.rs"},
            {"evidence":"the gate misled me","severity":"medium"}
        ]"#;
        let findings = parse_friction_findings(block);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].kind, FindingKind::Friction);
        assert_eq!(findings[0].severity, Severity::High);
        assert_eq!(findings[0].path.as_deref(), Some("a.rs"));
        // Defaults: missing confidence → 0.5, missing path → none.
        assert!((findings[1].confidence - 0.5).abs() < f32::EPSILON);
        assert!(findings[1].path.is_none());
    }

    #[test]
    fn unwraps_a_fenced_block() {
        let block =
            "Here is what I found:\n```json\n[{\"evidence\":\"missing tool\"}]\n```\nthanks";
        let findings = parse_friction_findings(block);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].evidence, "missing tool");
    }

    #[test]
    fn malformed_and_empty_degrade_safely() {
        assert!(parse_friction_findings("not json at all").is_empty());
        assert!(parse_friction_findings("").is_empty());
        assert!(parse_friction_findings("[]").is_empty());
        // An entry without evidence is dropped, not errored.
        assert!(parse_friction_findings(r#"[{"severity":"high"}]"#).is_empty());
    }
}
