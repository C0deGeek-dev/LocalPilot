//! Structured, deterministic context summaries.
//!
//! One shape serves every place the runtime condenses history into a bounded
//! digest: context compaction (trimmed exchanges) and harness branch closures
//! (abandoned step attempts). Keeping the format shared means a reader of the
//! event log or a compacted conversation learns one shape, and tests can pin
//! it once.

use serde::{Deserialize, Serialize};

/// Current summary contract version.
pub const STRUCTURED_SUMMARY_SCHEMA_VERSION: u32 = 2;

/// A bounded, factual digest: a title line and itemized entries. Deterministic
/// by construction — no model involvement.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredSummary {
    /// Contract version for durable event logs and future importers.
    #[serde(default = "default_summary_schema_version")]
    pub schema_version: u32,
    /// What this summary condenses (one line, ends with a colon by convention).
    pub title: String,
    /// Itemized entries, most relevant first. Rendered as `- ` bullets.
    pub entries: Vec<String>,
    /// Shared sectioned digest used by compaction, ingest, and memory adapters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<SummarySection>,
    /// Source references that ground the summary without storing raw transcript
    /// dumps.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SummarySource>,
    /// Budget and truncation metadata for audit and inspection.
    #[serde(default, skip_serializing_if = "SummaryBudget::is_empty")]
    pub budget: SummaryBudget,
}

impl StructuredSummary {
    /// A summary with a title and entries.
    #[must_use]
    pub fn new(title: impl Into<String>, entries: Vec<String>) -> Self {
        Self {
            schema_version: STRUCTURED_SUMMARY_SCHEMA_VERSION,
            title: title.into(),
            entries,
            sections: Vec::new(),
            sources: Vec::new(),
            budget: SummaryBudget::default(),
        }
    }

    /// Attach the shared structured section payload.
    #[must_use]
    pub fn with_sections(mut self, sections: Vec<SummarySection>) -> Self {
        self.sections = sections;
        self
    }

    /// Attach source hints.
    #[must_use]
    pub fn with_sources(mut self, sources: Vec<SummarySource>) -> Self {
        self.sources = sources;
        self
    }

    /// Render as plain text: the title line followed by `- ` bullets.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = self.title.clone();
        let entries = if self.entries.is_empty() && !self.sections.is_empty() {
            self.sections
                .iter()
                .filter(|section| !section.items.is_empty())
                .flat_map(|section| {
                    section
                        .items
                        .iter()
                        .map(|item| format!("{}: {item}", section.kind.label()))
                })
                .collect()
        } else {
            self.entries.clone()
        };
        for entry in &entries {
            out.push('\n');
            out.push_str("- ");
            out.push_str(entry);
        }
        out
    }
}

fn default_summary_schema_version() -> u32 {
    STRUCTURED_SUMMARY_SCHEMA_VERSION
}

/// Shared digest sections for derived context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummarySectionKind {
    Goal,
    Constraints,
    Progress,
    Decisions,
    NextSteps,
    CriticalContext,
    RelevantFiles,
    CommandOutcomes,
    Risks,
    StaleOrSuperseded,
}

impl SummarySectionKind {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Goal => "goal",
            Self::Constraints => "constraints/preferences",
            Self::Progress => "progress",
            Self::Decisions => "decisions",
            Self::NextSteps => "next steps",
            Self::CriticalContext => "critical context",
            Self::RelevantFiles => "relevant files",
            Self::CommandOutcomes => "commands/failures",
            Self::Risks => "unresolved risks",
            Self::StaleOrSuperseded => "stale/superseded",
        }
    }
}

/// One populated section in a structured digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummarySection {
    pub kind: SummarySectionKind,
    pub items: Vec<String>,
}

impl SummarySection {
    #[must_use]
    pub fn new(kind: SummarySectionKind, items: Vec<String>) -> Self {
        Self { kind, items }
    }
}

/// Source kind for a digest claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummarySourceKind {
    MessageRange,
    ToolCall,
    ToolResult,
    FilePath,
    Command,
    PreviousSummary,
    IngestChunk,
    AcceptedMemory,
    GraphFact,
}

/// Provenance for one summary claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummarySource {
    pub kind: SummarySourceKind,
    pub source_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub redacted: bool,
}

impl SummarySource {
    #[must_use]
    pub fn new(kind: SummarySourceKind, source_id: impl Into<String>) -> Self {
        Self {
            kind,
            source_id: source_id.into(),
            range: None,
            path: None,
            redacted: true,
        }
    }

    #[must_use]
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    #[must_use]
    pub fn with_range(mut self, range: impl Into<String>) -> Self {
        self.range = Some(range.into());
        self
    }
}

/// Budget and truncation metadata for a summary/projection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummaryBudget {
    pub estimated_tokens: usize,
    pub original_messages: usize,
    pub kept_messages: usize,
    pub dropped_messages: usize,
    pub truncated_tool_results: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation_reason: Option<String>,
}

impl SummaryBudget {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_title_and_bullets() {
        let summary = StructuredSummary::new(
            "Conversation summary for trimmed history:",
            vec![
                "user asked: fix the bug".to_string(),
                "tools used: read_file".to_string(),
            ],
        );
        assert_eq!(
            summary.render(),
            "Conversation summary for trimmed history:\n- user asked: fix the bug\n- tools used: read_file"
        );
    }

    #[test]
    fn empty_entries_render_title_only() {
        let summary = StructuredSummary::new("Nothing to report:", Vec::new());
        assert_eq!(summary.render(), "Nothing to report:");
    }

    #[test]
    fn roundtrips_through_serde() {
        let summary = StructuredSummary::new("t:", vec!["a".to_string()])
            .with_sections(vec![SummarySection::new(
                SummarySectionKind::Goal,
                vec!["ship context intelligence".to_string()],
            )])
            .with_sources(vec![SummarySource::new(
                SummarySourceKind::MessageRange,
                "messages",
            )
            .with_range("1..3")]);
        let json = serde_json::to_string(&summary).unwrap();
        let back: StructuredSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(summary, back);
    }
}
