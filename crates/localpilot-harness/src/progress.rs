//! `PROGRESS.md` parsing and rendering.
//!
//! Authoritative and user-editable. A user-edited file is accepted if it is
//! semantically valid; a malformed file reports the exact problem (for example a
//! duplicate step number). Rendering round-trips losslessly.

use serde::{Deserialize, Serialize};

use crate::brief::title_after;
use crate::error::HarnessError;

const DOCUMENT: &str = "PROGRESS.md";

/// How a step is verified, when the plan says.
///
/// "Nothing to run here" is a claim a person should have to make in words, so
/// the absence of a command carries a reason rather than being spelled as an
/// empty command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verification {
    /// The smallest relevant check for this step.
    Command(String),
    /// Nothing executable applies, and why.
    None { reason: String },
}

/// A single plan step.
///
/// The three planning fields — `covers`, `verify`, `depends` — are `Option`
/// because absence means UNKNOWN, not empty: every plan written before this
/// format existed has none of them, and a legacy plan must round-trip
/// byte-identically rather than acquire fields nobody decided. An explicit
/// "none" is a different fact and is spelled on disk (`covers: none`,
/// `depends: none`, `verify: none - <reason>`), so a reader can tell a decision
/// from a silence. Newly approved plans are required to carry all three; that
/// rule belongs to approval, not to parsing, because a user-edited file is
/// accepted whenever it is semantically valid.
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
    /// Acceptance criteria this step owns, by their numbers in the bound brief.
    /// `Some(empty)` is an explicit "this step owns none of them".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub covers: Option<Vec<usize>>,
    /// The step's verification, or a stated reason none applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<Verification>,
    /// Step numbers that must come first. `Some(empty)` is an explicit "nothing".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends: Option<Vec<usize>>,
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
                        "covers" => last.covers = Some(parse_numbers(value, true)?),
                        "depends" => last.depends = Some(parse_numbers(value, false)?),
                        "verify" => last.verify = Some(parse_verification(value)?),
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
            // Written only when present, for the same reason the binding is: a
            // plan that predates the format round-trips byte-identically rather
            // than gaining fields nobody decided.
            if let Some(covers) = &step.covers {
                out.push_str(&format!("  - covers: {}\n", render_numbers(covers, "AC")));
            }
            if let Some(verify) = &step.verify {
                let value = match verify {
                    Verification::Command(command) => command.clone(),
                    Verification::None { reason } => format!("none - {reason}"),
                };
                out.push_str(&format!("  - verify: {value}\n"));
            }
            if let Some(depends) = &step.depends {
                out.push_str(&format!("  - depends: {}\n", render_numbers(depends, "")));
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
            // `feature` appends a step nobody planned against a brief criterion,
            // so the three planning fields are UNKNOWN here rather than empty.
            // Claiming it covers nothing would be a decision this command has no
            // standing to make.
            covers: None,
            verify: None,
            depends: None,
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

/// A number list as it is written back, or the explicit `none`.
fn render_numbers(numbers: &[usize], prefix: &str) -> String {
    if numbers.is_empty() {
        return "none".to_string();
    }
    numbers
        .iter()
        .map(|number| format!("{prefix}{number}"))
        .collect::<Vec<_>>()
        .join(", ")
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
        // Filled by the metadata sub-bullets that follow the step line, if any.
        covers: None,
        verify: None,
        depends: None,
    }))
}

/// A comma-separated list of numbers, or the explicit word `none`.
///
/// `none` yields an empty list: the caller keeps it inside `Some`, which is what
/// distinguishes a stated "nothing" from a legacy silence.
///
/// `allow_ac` is the difference between the two vocabularies. A criterion may be
/// written `AC3` or `3`, because the render uses the prefix and a hand-editor
/// will type either. A dependency is a step number and never carries it —
/// accepting `AC3` there would let a plan appear to depend on a criterion, which
/// is a different kind of thing entirely. Nothing else is accepted: a parser
/// that took `C1`, `A1` or `ACAC1` would read a spelling it never writes back.
fn parse_numbers(value: &str, allow_ac: bool) -> Result<Vec<usize>, HarnessError> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }
    let expected = if allow_ac {
        "a list of criterion numbers like 'AC1, AC3' or '1, 3'"
    } else {
        "a list of step numbers like '1, 2'"
    };
    let mut out = Vec::new();
    for part in value.split(',') {
        let part = part.trim();
        let digits = match part.get(..2) {
            Some(head) if allow_ac && head.eq_ignore_ascii_case("ac") => &part[2..],
            _ => part,
        };
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(HarnessError::Malformed {
                document: DOCUMENT,
                detail: format!("'{value}' is not {expected}"),
            });
        }
        let number: usize = digits.parse().map_err(|_| HarnessError::Malformed {
            document: DOCUMENT,
            detail: format!("'{value}' is not {expected}"),
        })?;
        if number == 0 {
            return Err(HarnessError::Malformed {
                document: DOCUMENT,
                detail: format!("'{value}' contains 0; numbering starts at 1"),
            });
        }
        if !out.contains(&number) {
            out.push(number);
        }
    }
    Ok(out)
}

/// `none - <reason>` or a command.
///
/// A bare `none` is malformed on purpose: claiming a step needs no verification
/// is exactly the claim that should carry its reason into the document.
fn parse_verification(value: &str) -> Result<Verification, HarnessError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(HarnessError::Malformed {
            document: DOCUMENT,
            detail: "empty 'verify:' - name the check, or 'none - <why>'".to_string(),
        });
    }
    if let Some(reason) = strip_none_prefix(value) {
        if reason.trim().is_empty() {
            return Err(HarnessError::Malformed {
                document: DOCUMENT,
                detail: "'verify: none' needs a reason - write 'verify: none - <why>'".to_string(),
            });
        }
        return Ok(Verification::None {
            reason: reason.trim().to_string(),
        });
    }
    Ok(Verification::Command(value.to_string()))
}

/// The text after a leading `none` and its separator, when the value states one.
fn strip_none_prefix(value: &str) -> Option<&str> {
    value
        .get(..4)
        .filter(|head| head.eq_ignore_ascii_case("none"))?;
    let tail = &value[4..];
    for separator in [" - ", " — ", " -- ", ": "] {
        if let Some(reason) = tail.strip_prefix(separator) {
            return Some(reason);
        }
    }
    if tail.trim().is_empty() {
        return Some("");
    }
    None
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

    const PLANNED: &str = "# Progress: parser errors
Branch: feature/parser-errors

## Steps

- [ ] 1. Write the failing test
  - covers: AC1, AC3
  - verify: cargo test -p localpilot-harness parser
  - depends: none
- [ ] 2. Implement it
  - covers: none
  - verify: none - documentation only
  - depends: 1
";

    #[test]
    fn a_plan_without_the_planning_fields_round_trips_byte_identically() {
        // The legacy case, and the reason the fields are Option: a plan written
        // before this format must come back exactly as it went in, not acquire
        // fields nobody decided. Absence is UNKNOWN, never empty.
        let progress = Progress::parse(VALID).unwrap();
        for step in &progress.steps {
            assert_eq!(step.covers, None);
            assert_eq!(step.verify, None);
            assert_eq!(step.depends, None);
        }
        assert_eq!(progress.render(), VALID);
    }

    #[test]
    fn the_planning_fields_round_trip_and_keep_their_three_states_apart() {
        let progress = Progress::parse(PLANNED).unwrap();
        let first = &progress.steps[0];
        assert_eq!(first.covers, Some(vec![1, 3]));
        assert_eq!(
            first.verify,
            Some(Verification::Command(
                "cargo test -p localpilot-harness parser".to_string()
            ))
        );
        // Stated "nothing", which is a different fact from the legacy silence
        // above and is spelled on disk so a reader can tell them apart.
        assert_eq!(first.depends, Some(Vec::new()));

        let second = &progress.steps[1];
        assert_eq!(second.covers, Some(Vec::new()));
        assert_eq!(
            second.verify,
            Some(Verification::None {
                reason: "documentation only".to_string()
            })
        );
        assert_eq!(second.depends, Some(vec![1]));

        assert_eq!(progress.render(), PLANNED);
        assert_eq!(Progress::parse(&progress.render()).unwrap(), progress);
    }

    #[test]
    fn a_bare_verify_none_is_malformed() {
        // "Nothing to run here" is a claim, and a claim carries its reason into
        // the document rather than being spelled as an empty command.
        let text = PLANNED.replace("none - documentation only", "none");
        let error = Progress::parse(&text).unwrap_err().to_string();
        assert!(error.contains("needs a reason"), "{error}");
    }

    #[test]
    fn the_two_number_vocabularies_are_not_interchangeable() {
        // A criterion may be written AC3 or 3; a dependency is a step number and
        // never carries the prefix. A parser that stripped leading A/C also took
        // C1, A1 and ACAC1, then wrote back a spelling it had never read.
        let with_plain = PLANNED.replace("covers: AC1, AC3", "covers: 1, 3");
        assert_eq!(
            Progress::parse(&with_plain).unwrap().steps[0].covers,
            Some(vec![1, 3])
        );

        for bad in ["covers: C1", "covers: A1", "covers: ACAC1", "depends: AC1"] {
            let (key, value) = bad.split_once(": ").unwrap();
            let text = PLANNED.replace(
                if key == "covers" {
                    "covers: AC1, AC3"
                } else {
                    "depends: 1"
                },
                &format!("{key}: {value}"),
            );
            let error = Progress::parse(&text).unwrap_err().to_string();
            assert!(error.contains("is not a list"), "{bad}: {error}");
        }
    }

    #[test]
    fn an_empty_verification_is_malformed_rather_than_an_empty_command() {
        // Left as Command("") it would satisfy a presence check at approval
        // while saying nothing at all.
        let text = PLANNED.replace("verify: none - documentation only", "verify:");
        let error = Progress::parse(&text).unwrap_err().to_string();
        assert!(error.contains("empty 'verify:'"), "{error}");
    }

    #[test]
    fn criterion_and_dependency_lists_refuse_what_is_not_a_number() {
        for bad in ["covers: everything", "depends: later"] {
            let (key, value) = bad.split_once(": ").unwrap();
            let text = PLANNED.replace(
                match key {
                    "covers" => "covers: AC1, AC3",
                    _ => "depends: 1",
                },
                &format!("{key}: {value}"),
            );
            let error = Progress::parse(&text).unwrap_err().to_string();
            assert!(error.contains("not a list"), "{bad}: {error}");
        }

        let zeroed = PLANNED.replace("covers: AC1, AC3", "covers: AC0");
        let error = Progress::parse(&zeroed).unwrap_err().to_string();
        assert!(error.contains("numbering starts at 1"), "{error}");
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
