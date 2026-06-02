//! `PROGRESS.md` parsing and rendering.
//!
//! Authoritative and user-editable. A user-edited file is accepted if it is
//! semantically valid; a malformed file reports the exact problem (for example a
//! duplicate step number). Rendering round-trips losslessly.

use serde::{Deserialize, Serialize};

use crate::brief::title_after;
use crate::error::HarnessError;

const DOCUMENT: &str = "PROGRESS.md";

/// A single plan step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub number: usize,
    pub description: String,
    pub done: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default)]
    pub attempts: u32,
}

/// A parsed `PROGRESS.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub name: String,
    pub branch: String,
    pub steps: Vec<Step>,
}

impl Progress {
    /// Parse progress from markdown text.
    ///
    /// # Errors
    /// Returns [`HarnessError::Malformed`] for a missing title/branch or a
    /// duplicate step number.
    pub fn parse(text: &str) -> Result<Self, HarnessError> {
        let text = text.replace("\r\n", "\n");
        let name = title_after(&text, "# Progress:").ok_or_else(|| HarnessError::Malformed {
            document: DOCUMENT,
            detail: "missing '# Progress: <name>' title".to_string(),
        })?;
        let branch = title_after(&text, "Branch:").ok_or_else(|| HarnessError::Malformed {
            document: DOCUMENT,
            detail: "missing 'Branch:' line".to_string(),
        })?;

        let mut steps: Vec<Step> = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim_start();
            if let Some(step) = parse_step_line(trimmed)? {
                if steps.iter().any(|s| s.number == step.number) {
                    return Err(HarnessError::Malformed {
                        document: DOCUMENT,
                        detail: format!("duplicate step number {}", step.number),
                    });
                }
                steps.push(step);
            } else if let Some((key, value)) = parse_meta_line(trimmed) {
                if let Some(last) = steps.last_mut() {
                    match key {
                        "commit" => last.commit = Some(value.to_string()),
                        "attempts" => {
                            last.attempts = value.parse().map_err(|_| HarnessError::Malformed {
                                document: DOCUMENT,
                                detail: format!("invalid attempts value '{value}'"),
                            })?;
                        }
                        _ => {}
                    }
                }
            }
        }

        Ok(Self {
            name,
            branch,
            steps,
        })
    }

    /// Render progress back to markdown, losslessly.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!(
            "# Progress: {}\nBranch: {}\n\n## Steps\n\n",
            self.name, self.branch
        );
        for step in &self.steps {
            let check = if step.done { "x" } else { " " };
            out.push_str(&format!(
                "- [{check}] {}. {}\n",
                step.number, step.description
            ));
            if step.done {
                if let Some(commit) = &step.commit {
                    out.push_str(&format!("  - commit: {commit}\n"));
                }
                if step.attempts > 0 {
                    out.push_str(&format!("  - attempts: {}\n", step.attempts));
                }
            }
        }
        out
    }

    /// The first step that is not yet done.
    #[must_use]
    pub fn next_incomplete(&self) -> Option<&Step> {
        self.steps.iter().find(|s| !s.done)
    }

    /// The number of completed steps.
    #[must_use]
    pub fn completed_count(&self) -> usize {
        self.steps.iter().filter(|s| s.done).count()
    }

    /// Append a new step after the highest existing number, leaving existing
    /// steps (and their completion metadata) untouched. Returns the new number.
    pub fn append_step(&mut self, description: impl Into<String>) -> usize {
        let number = self.steps.iter().map(|s| s.number).max().unwrap_or(0) + 1;
        self.steps.push(Step {
            number,
            description: description.into(),
            done: false,
            commit: None,
            attempts: 0,
        });
        number
    }

    /// Mark a step complete, recording its commit and attempt count.
    pub fn mark_complete(&mut self, number: usize, commit: Option<String>, attempts: u32) -> bool {
        if let Some(step) = self.steps.iter_mut().find(|s| s.number == number) {
            step.done = true;
            step.commit = commit;
            step.attempts = attempts;
            true
        } else {
            false
        }
    }
}

fn parse_step_line(line: &str) -> Result<Option<Step>, HarnessError> {
    let rest = match line.strip_prefix("- [") {
        Some(rest) => rest,
        None => return Ok(None),
    };
    let (mark, after) = rest.split_at(rest.chars().next().map_or(0, char::len_utf8));
    let done = match mark {
        "x" | "X" => true,
        " " => false,
        _ => return Ok(None),
    };
    let after = after
        .strip_prefix("] ")
        .ok_or_else(|| HarnessError::Malformed {
            document: DOCUMENT,
            detail: format!("malformed step checkbox: {line}"),
        })?;
    let (number_str, description) =
        after
            .split_once(". ")
            .ok_or_else(|| HarnessError::Malformed {
                document: DOCUMENT,
                detail: format!("step missing 'N. description': {line}"),
            })?;
    let number = number_str
        .trim()
        .parse()
        .map_err(|_| HarnessError::Malformed {
            document: DOCUMENT,
            detail: format!("invalid step number '{number_str}'"),
        })?;
    Ok(Some(Step {
        number,
        description: description.trim().to_string(),
        done,
        commit: None,
        attempts: 0,
    }))
}

fn parse_meta_line(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix("- ")?;
    let (key, value) = rest.split_once(':')?;
    Some((key.trim(), value.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "# Progress: parser errors\nBranch: feature/parser-errors\n\n## Steps\n\n\
- [x] 1. Write failing test for parser errors\n  - commit: abc1234\n  - attempts: 1\n\
- [ ] 2. Implement parser errors\n- [ ] 3. Document parser errors\n";

    #[test]
    fn parses_valid_progress() {
        let progress = Progress::parse(VALID).unwrap();
        assert_eq!(progress.branch, "feature/parser-errors");
        assert_eq!(progress.steps.len(), 3);
        assert!(progress.steps[0].done);
        assert_eq!(progress.steps[0].commit.as_deref(), Some("abc1234"));
        assert_eq!(progress.steps[0].attempts, 1);
        assert_eq!(progress.next_incomplete().map(|s| s.number), Some(2));
        assert_eq!(progress.completed_count(), 1);
    }

    #[test]
    fn rejects_duplicate_step_numbers() {
        let dup = VALID.replace("- [ ] 3.", "- [ ] 2.");
        let err = Progress::parse(&dup).unwrap_err();
        assert!(
            matches!(err, HarnessError::Malformed { detail, .. } if detail.contains("duplicate"))
        );
    }

    #[test]
    fn render_round_trip_is_lossless() {
        let progress = Progress::parse(VALID).unwrap();
        let reparsed = Progress::parse(&progress.render()).unwrap();
        assert_eq!(progress, reparsed);
    }

    #[test]
    fn append_step_leaves_existing_steps_and_metadata_intact() {
        let mut progress = Progress::parse(VALID).unwrap();
        let number = progress.append_step("New feature step");
        assert_eq!(number, 4);
        // Existing completed step 1 keeps its commit and attempts.
        assert_eq!(progress.steps[0].commit.as_deref(), Some("abc1234"));
        assert_eq!(progress.steps[0].attempts, 1);
        assert!(progress.steps[0].done);
        // The new step is last and incomplete.
        assert_eq!(progress.steps.last().unwrap().number, 4);
        assert!(!progress.steps.last().unwrap().done);
    }

    #[test]
    fn mark_complete_updates_a_step() {
        let mut progress = Progress::parse(VALID).unwrap();
        assert!(progress.mark_complete(2, Some("def5678".to_string()), 2));
        assert_eq!(progress.next_incomplete().map(|s| s.number), Some(3));
    }
}
