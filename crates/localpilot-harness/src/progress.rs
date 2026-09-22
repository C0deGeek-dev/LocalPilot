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
    /// The sessions that worked this step, oldest first, carried as a
    /// `sessions:` line on a completed step. More than one when the step was
    /// paused or blocked and resumed. Empty for a step completed before the
    /// link existed, which is *unknown*, not "no session".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<String>,
}

/// A parsed `PROGRESS.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub name: String,
    pub branch: String,
    /// The brief revision this plan was generated from or adopted against,
    /// carried as a `Brief:` header line. `None` for a plan written before the
    /// binding existed, which is a distinct condition from a binding that no
    /// longer matches: unbound is *unknown*, stale is *known wrong*.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brief_binding: Option<String>,
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
        // Only the header — everything before the first `## ` heading — may carry
        // the binding, so a step description that happens to contain `Brief:`
        // cannot be mistaken for one.
        let header: Vec<&str> = text
            .lines()
            .take_while(|line| !line.trim_start().starts_with("## "))
            .collect();
        // Two binding lines have no defined meaning, and silently taking the
        // first would let a hand-edit or a bad merge decide which requirements a
        // plan claims to satisfy. Every occurrence counts, including an empty
        // one: the question is how many times the document tries to say what it
        // is bound to, not how many of those attempts carry a value.
        let bindings: Vec<&str> = header
            .iter()
            .filter_map(|line| line.trim().strip_prefix("Brief:"))
            .map(str::trim)
            .collect();
        if bindings.len() > 1 {
            return Err(HarnessError::Malformed {
                document: DOCUMENT,
                detail: format!(
                    "{} 'Brief:' lines; a plan records exactly one brief revision",
                    bindings.len()
                ),
            });
        }
        // A present-but-empty header is a broken binding, not the absence of
        // one. Treating it as legacy-unbound would make a damaged document
        // adoptable, quietly binding a plan whose recorded revision was lost.
        if bindings.first().is_some_and(|value| value.is_empty()) {
            return Err(HarnessError::Malformed {
                document: DOCUMENT,
                detail: "empty 'Brief:' line; a binding records a brief revision".to_string(),
            });
        }
        let brief_binding = bindings.first().map(|value| (*value).to_string());

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
                        "sessions" => last.sessions.extend(
                            value
                                .split(',')
                                .map(str::trim)
                                .filter(|id| !id.is_empty())
                                .map(str::to_string),
                        ),
                        _ => {}
                    }
                }
            }
        }

        Ok(Self {
            name,
            branch,
            brief_binding,
            steps,
        })
    }

    /// Render progress back to markdown, losslessly.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!("# Progress: {}\nBranch: {}\n", self.name, self.branch);
        // The binding is written only when present, so a plan that predates it
        // round-trips byte-identically instead of gaining an empty field.
        if let Some(binding) = &self.brief_binding {
            out.push_str(&format!("Brief: {binding}\n"));
        }
        out.push_str("\n## Steps\n\n");
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
                if !step.sessions.is_empty() {
                    out.push_str(&format!("  - sessions: {}\n", step.sessions.join(", ")));
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

    /// Whether the step with `number` is marked done in this snapshot. Used to
    /// check, after a turn, whether the model actually reflected the completion
    /// it claimed in `PROGRESS.md`.
    #[must_use]
    pub fn step_is_done(&self, number: usize) -> bool {
        self.steps.iter().any(|s| s.number == number && s.done)
    }

    /// Record the brief revision this plan is bound to, leaving every step and
    /// all of its completion evidence untouched.
    pub fn bind_to_brief(&mut self, revision: impl Into<String>) {
        self.brief_binding = Some(revision.into());
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
            sessions: Vec::new(),
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

    /// Record the sessions that worked a step, replacing any recorded before.
    /// Returns `false` when no step has that number.
    pub fn record_sessions(&mut self, number: usize, sessions: Vec<String>) -> bool {
        if let Some(step) = self.steps.iter_mut().find(|s| s.number == number) {
            step.sessions = sessions;
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
        sessions: Vec::new(),
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
    fn step_is_done_tracks_the_checkbox_state() {
        let progress = Progress::parse(VALID).unwrap();
        assert!(progress.step_is_done(1), "step 1 is checked");
        assert!(!progress.step_is_done(2), "step 2 is unchecked");
        assert!(!progress.step_is_done(99), "an unknown step is not done");
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

    const LINKED: &str = "# Progress: parser errors\nBranch: feature/parser-errors\n\n## Steps\n\n\
- [x] 1. Write failing test for parser errors\n  - commit: abc1234\n  - attempts: 1\n  - sessions: \
0b0e6c1e-0000-4000-8000-000000000001, 0b0e6c1e-0000-4000-8000-000000000002\n\
- [ ] 2. Implement parser errors\n";

    #[test]
    fn a_step_carries_the_sessions_that_worked_it_in_order() {
        let progress = Progress::parse(LINKED).unwrap();
        assert_eq!(
            progress.steps[0].sessions,
            vec![
                "0b0e6c1e-0000-4000-8000-000000000001",
                "0b0e6c1e-0000-4000-8000-000000000002"
            ]
        );
        assert_eq!(progress.render(), LINKED, "and renders back byte for byte");
    }

    #[test]
    fn a_plan_written_before_the_link_round_trips_unchanged() {
        // No `sessions:` line is invented for a step that never had one: its
        // sessions are unknown, and the file says nothing rather than "none".
        let progress = Progress::parse(VALID).unwrap();
        assert!(progress.steps[0].sessions.is_empty());
        assert_eq!(progress.render(), VALID);
    }

    #[test]
    fn recording_sessions_leaves_commit_and_attempts_alone() {
        let mut progress = Progress::parse(VALID).unwrap();
        assert!(progress.mark_complete(2, Some("def5678".to_string()), 2));
        assert!(progress.record_sessions(2, vec!["s-1".to_string()]));
        assert!(!progress.record_sessions(99, vec!["s-1".to_string()]));
        let step = &Progress::parse(&progress.render()).unwrap().steps[1];
        assert_eq!(step.commit.as_deref(), Some("def5678"));
        assert_eq!(step.attempts, 2);
        assert_eq!(step.sessions, vec!["s-1"]);
    }

    #[test]
    fn mark_complete_updates_a_step() {
        let mut progress = Progress::parse(VALID).unwrap();
        assert!(progress.mark_complete(2, Some("def5678".to_string()), 2));
        assert_eq!(progress.next_incomplete().map(|s| s.number), Some(3));
    }
}
