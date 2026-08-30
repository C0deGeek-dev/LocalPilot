//! The pure finding → outward-draft-spec mapping.
//!
//! The read-only half of authoring an outward draft (ADR-0053): given a ranked
//! [`Finding`], produce the human-readable parts of a draft issue/PR describing it
//! — a title, a description, and the provenance fields (source, rationale, risks).
//! It is **data only**: it touches no network, mints no token, and depends on
//! nothing outside this read-only crate. The host (CLI) folds the spec into the
//! gated `localpilot-patchgen` `OutwardDraft`, which applies the allowlist policy,
//! redaction, and the publish gate.

use crate::finding::{Finding, Risk};

/// The human-authored parts of an outward draft, derived purely from a finding.
/// Carries the provenance components so the gated artefact can render a
/// traceable body without re-deriving them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutwardDraftSpec {
    /// A short, single-line title for the issue/PR.
    pub title: String,
    /// The human-readable description (the body before the provenance block).
    pub description: String,
    /// Provenance: what produced this draft (the finding and its source).
    pub source: String,
    /// Provenance: why it is worth acting on (the recommended action, when known).
    pub rationale: String,
    /// Provenance: how risky acting on the finding is.
    pub risks: String,
}

/// Build the draft spec for a ranked `finding`. Returns `None` only when the
/// finding carries no evidence to describe (so there is nothing to propose).
#[must_use]
pub fn draft_spec_for_finding(finding: &Finding) -> Option<OutwardDraftSpec> {
    let evidence = finding.evidence.trim();
    if evidence.is_empty() {
        return None;
    }
    let location = match (&finding.path, &finding.span) {
        (Some(path), Some(span)) => format!("{path}:{}", span.start_line),
        (Some(path), None) => path.clone(),
        _ => "(no specific file)".to_string(),
    };
    let title = title_from(evidence, &finding.path);
    let mut description = String::new();
    use std::fmt::Write as _;
    let _ = writeln!(
        description,
        "A read-only self-review surfaced a `{:?}` finding ({:?} severity, {:.0}% confidence).",
        finding.kind,
        finding.severity,
        finding.confidence * 100.0
    );
    let _ = writeln!(description);
    let _ = writeln!(description, "- location: {location}");
    let _ = writeln!(description, "- evidence: {evidence}");
    if let Some(action) = &finding.recommendation {
        let _ = writeln!(description, "- recommended: {action}");
    }
    let rationale = finding
        .recommendation
        .clone()
        .unwrap_or_else(|| format!("Address the self-review finding: {evidence}"));
    let risks = match finding.risk {
        Risk::Low => "low — acting on it is unlikely to change behaviour".to_string(),
        Risk::Medium => "medium — verify before acting".to_string(),
        Risk::High => "high — behaviour-sensitive; validate carefully".to_string(),
    };
    Some(OutwardDraftSpec {
        title,
        description,
        source: format!("self-review {:?} finding at {location}", finding.kind),
        rationale,
        risks,
    })
}

/// A concise single-line title: a path prefix (when present) plus a trimmed slice
/// of the evidence, collapsed to one line and capped so it stays a title.
fn title_from(evidence: &str, path: &Option<String>) -> String {
    let one_line: String = evidence.split_whitespace().collect::<Vec<_>>().join(" ");
    let summary: String = one_line.chars().take(72).collect();
    match path {
        Some(p) => format!("self-review: {summary} ({p})"),
        None => format!("self-review: {summary}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::{Finding, FindingKind, Severity, Span};

    #[test]
    fn a_spec_is_built_from_a_finding_and_carries_provenance() {
        let finding = Finding::new(
            FindingKind::Todo,
            Severity::Low,
            0.9,
            "stale TODO marker left in a tracked file".to_string(),
        )
        .at_path("src/a.rs")
        .at_span(Span::line(12))
        .recommending("track it in the issue tracker or remove it");

        let spec = draft_spec_for_finding(&finding).expect("a finding with evidence yields a spec");
        // Title is one line and mentions the file.
        assert!(spec.title.starts_with("self-review:"));
        assert!(spec.title.contains("src/a.rs"));
        assert!(!spec.title.contains('\n'));
        // Description grounds the finding in its location + evidence.
        assert!(spec.description.contains("src/a.rs:12"));
        assert!(spec.description.contains("stale TODO marker"));
        assert!(spec.description.contains("track it in the issue tracker"));
        // Provenance components are present: source, rationale, risks.
        assert!(spec.source.contains("self-review"));
        assert!(spec.source.contains("src/a.rs:12"));
        assert_eq!(spec.rationale, "track it in the issue tracker or remove it");
        assert!(spec.risks.starts_with("low"));
    }

    #[test]
    fn a_finding_without_a_recommendation_falls_back_to_addressing_it() {
        let finding = Finding::new(
            FindingKind::DocDrift,
            Severity::Medium,
            0.8,
            "doc links to a missing file".to_string(),
        )
        .at_path("docs/x.md");
        let spec = draft_spec_for_finding(&finding).unwrap();
        assert!(spec.rationale.contains("Address the self-review finding"));
        assert!(spec.rationale.contains("doc links to a missing file"));
    }

    #[test]
    fn a_finding_with_no_evidence_yields_no_spec() {
        let finding = Finding::new(
            FindingKind::Friction,
            Severity::Info,
            0.5,
            "   ".to_string(),
        );
        assert!(draft_spec_for_finding(&finding).is_none());
    }

    #[test]
    fn risk_level_maps_to_a_human_note() {
        let high = Finding::new(
            FindingKind::DeadCode,
            Severity::Low,
            0.7,
            "maybe dead".to_string(),
        )
        .with_risk(Risk::High)
        .at_path("src/x.rs");
        assert!(draft_spec_for_finding(&high)
            .unwrap()
            .risks
            .starts_with("high"));
    }
}
