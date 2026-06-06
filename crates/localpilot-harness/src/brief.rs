//! `brief.md` parsing and rendering.
//!
//! The brief is authoritative and user-editable; the next run treats the edited
//! file as truth. Parsing accepts both `\n` and `\r\n` and reports a missing
//! required section by name.

use serde::{Deserialize, Serialize};

use crate::error::HarnessError;

const DOCUMENT: &str = "brief.md";

/// A parsed `brief.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Brief {
    pub name: String,
    pub summary: String,
    pub requirements: Vec<String>,
    pub constraints: Vec<String>,
    pub non_goals: Vec<String>,
    pub acceptance_criteria: Vec<String>,
}

impl Brief {
    /// Parse a brief from markdown text.
    ///
    /// # Errors
    /// Returns [`HarnessError::MissingSection`] if a required section is absent,
    /// or [`HarnessError::Malformed`] if the title line is missing.
    pub fn parse(text: &str) -> Result<Self, HarnessError> {
        let text = text.replace("\r\n", "\n");
        let name = title_after(&text, "# Brief:").ok_or_else(|| HarnessError::Malformed {
            document: DOCUMENT,
            detail: "missing '# Brief: <name>' title".to_string(),
        })?;

        let sections = split_sections(&text);
        let summary = require(&sections, "Summary")?.join("\n").trim().to_string();
        let requirements = bullet_items(require(&sections, "Requirements")?);
        let constraints = bullet_items(require(&sections, "Constraints")?);
        let non_goals = bullet_items(require(&sections, "Non-Goals")?);
        let acceptance_criteria = bullet_items(require(&sections, "Acceptance Criteria")?);

        Ok(Self {
            name,
            summary,
            requirements,
            constraints,
            non_goals,
            acceptance_criteria,
        })
    }

    /// Append a requirement note (used when a feature is added to an existing
    /// brief), leaving the rest of the brief untouched.
    pub fn add_requirement(&mut self, text: impl Into<String>) {
        self.requirements.push(text.into());
    }

    /// Render the brief back to markdown.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!("# Brief: {}\n\n", self.name);
        out.push_str("## Summary\n\n");
        if !self.summary.is_empty() {
            out.push_str(&self.summary);
            out.push_str("\n\n");
        }
        render_list(&mut out, "Requirements", &self.requirements);
        render_list(&mut out, "Constraints", &self.constraints);
        render_list(&mut out, "Non-Goals", &self.non_goals);
        render_list(&mut out, "Acceptance Criteria", &self.acceptance_criteria);
        out
    }
}

fn render_list(out: &mut String, header: &str, items: &[String]) {
    out.push_str(&format!("## {header}\n\n"));
    for item in items {
        out.push_str(&format!("- {item}\n"));
    }
    out.push('\n');
}

pub(crate) fn title_after(text: &str, prefix: &str) -> Option<String> {
    text.lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix(prefix)
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
}

/// Split a document into `## Section` → body-lines.
pub(crate) fn split_sections(text: &str) -> Vec<(String, Vec<String>)> {
    let mut sections: Vec<(String, Vec<String>)> = Vec::new();
    for line in text.lines() {
        if let Some(header) = line.trim().strip_prefix("## ") {
            sections.push((header.trim().to_string(), Vec::new()));
        } else if let Some((_, body)) = sections.last_mut() {
            body.push(line.to_string());
        }
    }
    sections
}

fn require<'a>(
    sections: &'a [(String, Vec<String>)],
    name: &str,
) -> Result<&'a [String], HarnessError> {
    sections
        .iter()
        .find(|(header, _)| header == name)
        .map(|(_, body)| body.as_slice())
        .ok_or_else(|| HarnessError::MissingSection {
            document: DOCUMENT,
            section: name.to_string(),
        })
}

pub(crate) fn bullet_items(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter_map(|line| {
            let trimmed = line.trim();
            trimmed
                .strip_prefix("- ")
                .or_else(|| trimmed.strip_prefix("* "))
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "# Brief: parser errors\n\n\
## Summary\n\nMake the parser report precise errors.\n\n\
## Requirements\n\n- Report the offending line\n- Name the section\n\n\
## Constraints\n\n- No new dependencies\n\n\
## Non-Goals\n\n- Rewriting the lexer\n\n\
## Acceptance Criteria\n\n- A malformed file names its section\n";

    #[test]
    fn parses_a_valid_brief() {
        let brief = Brief::parse(VALID).unwrap();
        assert_eq!(brief.name, "parser errors");
        assert_eq!(brief.requirements.len(), 2);
        assert_eq!(brief.constraints, vec!["No new dependencies"]);
        assert!(brief.summary.contains("precise errors"));
    }

    #[test]
    fn rejects_a_brief_missing_a_section_naming_it() {
        let text = VALID.replace("## Constraints\n\n- No new dependencies\n\n", "");
        let err = Brief::parse(&text).unwrap_err();
        match err {
            HarnessError::MissingSection { section, .. } => assert_eq!(section, "Constraints"),
            other => panic!("expected MissingSection, got {other:?}"),
        }
    }

    #[test]
    fn accepts_crlf_line_endings() {
        let crlf = VALID.replace('\n', "\r\n");
        assert!(Brief::parse(&crlf).is_ok());
    }

    #[test]
    fn render_round_trips_through_parse() {
        let brief = Brief::parse(VALID).unwrap();
        let reparsed = Brief::parse(&brief.render()).unwrap();
        assert_eq!(brief, reparsed);
    }
}
