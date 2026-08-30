//! Completion retrospective: an advisory end-of-run review.
//!
//! When a plan has no incomplete step left, the harness asks the model to look
//! back over the brief and the completed plan and report what is clearer now than
//! before the work started: acceptance criteria still unmet, scope drift, tests
//! that pin implementation detail rather than behaviour, and lessons worth
//! keeping. It is advisory — it reports findings and records lessons to
//! `LESSONS.md`; it never blocks completion, edits shipped code, or commits. It
//! runs once, after the final step is already committed by the step loop.

use std::path::Path;

use localpilot_core::{Message, Role};
use localpilot_llm::ModelProvider;

use crate::brief::{bullet_items, split_sections, Brief};
use crate::decisions::today;
use crate::error::HarnessError;
use crate::lessons::Lessons;
use crate::planning::complete_text;
use crate::progress::Progress;

/// The original LocalPilot completion-retrospective prompt.
pub const RETROSPECTIVE_PROMPT: &str = "\
You are reviewing a finished piece of work with hindsight. You are given the \
project brief and the completed implementation plan. Assume the work is done, then \
report what is clearer now than before it started.\n\
\n\
Respond with ONLY a Markdown document in exactly this shape, and nothing else:\n\
\n\
# Retrospective: <short name>\n\
\n\
## Unmet Acceptance Criteria\n\
- <an acceptance criterion from the brief the completed work does not satisfy>\n\
\n\
## Lessons\n\
- <a durable lesson worth keeping for future work>\n\
\n\
## Notes\n\
<one short paragraph on scope drift, or on tests that pin implementation detail \
instead of observable behaviour>\n\
\n\
Under any heading with nothing to report, write a single bullet that reads \
'none'. Be specific and short.";

/// A parsed completion retrospective.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Retrospective {
    /// Acceptance criteria the review judged unsatisfied by the shipped work.
    pub unmet_criteria: Vec<String>,
    /// Durable lessons worth keeping; appended to `LESSONS.md`.
    pub lessons: Vec<String>,
    /// Free-text notes (scope drift, test-quality observations).
    pub notes: String,
}

impl Retrospective {
    /// Parse a retrospective from the model's markdown reply.
    ///
    /// Lenient by design: a missing title or section yields empty fields rather
    /// than an error, so a malformed review degrades to "no findings" and never
    /// breaks a finished run. A heading whose only bullet is `none` is empty.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let text = text.replace("\r\n", "\n");
        let sections = split_sections(&text);
        let section = |name: &str| {
            sections
                .iter()
                .find(|(header, _)| header == name)
                .map(|(_, body)| body.as_slice())
                .unwrap_or(&[])
        };
        let unmet_criteria = drop_none(bullet_items(section("Unmet Acceptance Criteria")));
        let lessons = drop_none(bullet_items(section("Lessons")));
        let notes = section("Notes").join("\n").trim().to_string();
        let notes = if notes.eq_ignore_ascii_case("none") {
            String::new()
        } else {
            notes
        };
        Self {
            unmet_criteria,
            lessons,
            notes,
        }
    }

    /// Whether the review surfaced an unmet acceptance criterion.
    #[must_use]
    pub fn has_findings(&self) -> bool {
        !self.unmet_criteria.is_empty()
    }

    /// A bounded, host-printable summary of the review.
    #[must_use]
    pub fn render_summary(&self) -> String {
        let mut s = String::from("retrospective:");
        if self.unmet_criteria.is_empty() {
            s.push_str(" all acceptance criteria addressed");
        } else {
            s.push_str(&format!(
                " {} acceptance criterion(s) still unmet:",
                self.unmet_criteria.len()
            ));
            for c in &self.unmet_criteria {
                s.push_str(&format!("\n  - {c}"));
            }
        }
        if !self.lessons.is_empty() {
            s.push_str(&format!(
                "\n  {} lesson(s) recorded to LESSONS.md",
                self.lessons.len()
            ));
        }
        s
    }
}

/// Run one bounded review call over the brief and completed plan.
///
/// Mirrors `run_plan`'s single-call shape but never hard-retries: an unparseable
/// reply yields an empty (no-findings) [`Retrospective`] rather than an error, so
/// a finished run is never broken by the review.
///
/// # Errors
/// Returns [`HarnessError::Provider`] only if the provider call itself fails; the
/// caller (a completed run) swallows it.
pub async fn run_retrospective(
    provider: &dyn ModelProvider,
    model: &str,
    brief: &Brief,
    progress: &Progress,
) -> Result<Retrospective, HarnessError> {
    let user = format!(
        "Project brief:\n\n{}\n\nCompleted plan:\n\n{}",
        brief.render(),
        progress.render()
    );
    let messages = vec![
        Message::text(Role::System, RETROSPECTIVE_PROMPT),
        Message::text(Role::User, user),
    ];
    let text = complete_text(provider, model, messages).await?;
    Ok(Retrospective::parse(&text))
}

/// Read the run's `brief.md` + `PROGRESS.md` from `root`, run the review, append
/// any lessons to `LESSONS.md`, and return the retrospective for the host to
/// surface.
///
/// Best-effort on inputs: returns `Ok(None)` when there is no `brief.md`, no
/// parseable plan, or no completed step — there is nothing to review. The only
/// side effect is appending to `LESSONS.md`; it makes no code edit and runs no
/// commit.
///
/// # Errors
/// Returns [`HarnessError`] only if the provider call fails or `LESSONS.md` cannot
/// be written; a completed run swallows it.
pub async fn run_and_record(
    provider: &dyn ModelProvider,
    model: &str,
    root: &Path,
) -> Result<Option<Retrospective>, HarnessError> {
    let Some(brief) = std::fs::read_to_string(root.join("brief.md"))
        .ok()
        .and_then(|t| Brief::parse(&t).ok())
    else {
        return Ok(None);
    };
    let Some(progress) = std::fs::read_to_string(root.join("PROGRESS.md"))
        .ok()
        .and_then(|t| Progress::parse(&t).ok())
    else {
        return Ok(None);
    };
    if progress.completed_count() == 0 {
        return Ok(None);
    }

    let retro = run_retrospective(provider, model, &brief, &progress).await?;
    if !retro.lessons.is_empty() {
        append_lessons(root, &progress.name, &retro.lessons)?;
    }
    Ok(Some(retro))
}

/// Append lessons to the root `LESSONS.md`, creating it on the first lesson.
fn append_lessons(root: &Path, name: &str, lessons: &[String]) -> Result<(), HarnessError> {
    let path = root.join("LESSONS.md");
    let mut log = match std::fs::read_to_string(&path) {
        Ok(text) => Lessons::parse(&text)?,
        Err(_) => Lessons::new(name),
    };
    let date = today();
    for lesson in lessons {
        log.append(date.clone(), lesson.clone());
    }
    std::fs::write(&path, log.render()).map_err(|source| HarnessError::Io {
        path: path.display().to_string(),
        source,
    })
}

fn drop_none(items: Vec<String>) -> Vec<String> {
    items
        .into_iter()
        .filter(|s| !s.eq_ignore_ascii_case("none"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_llm::FakeProvider;

    const BRIEF: &str = "# Brief: greeting\n\n## Summary\n\nGreet the user.\n\n\
## Requirements\n\n- Print a greeting\n\n## Constraints\n\n- Keep it small\n\n\
## Non-Goals\n\n- Internationalization\n\n## Acceptance Criteria\n\n\
- A greeting is printed\n- The greeting names the user\n";

    const DONE_PLAN: &str = "# Progress: greeting\nBranch: feature/greeting\n\n## Steps\n\n\
- [x] 1. Print a greeting\n  - commit: abc1234\n  - attempts: 1\n";

    const REVIEW_WITH_GAP: &str = "# Retrospective: greeting\n\n\
## Unmet Acceptance Criteria\n- The greeting names the user\n\n\
## Lessons\n- Wire the user's name through before claiming the criterion\n\n\
## Notes\nThe name path was never added.\n";

    const REVIEW_CLEAN: &str = "# Retrospective: greeting\n\n\
## Unmet Acceptance Criteria\n- none\n\n## Lessons\n- none\n\n## Notes\nnone\n";

    #[test]
    fn parse_extracts_unmet_criteria_and_lessons() {
        let retro = Retrospective::parse(REVIEW_WITH_GAP);
        assert_eq!(retro.unmet_criteria, vec!["The greeting names the user"]);
        assert_eq!(retro.lessons.len(), 1);
        assert!(retro.has_findings());
        assert!(retro.notes.contains("name path"));
    }

    #[test]
    fn parse_treats_none_bullets_as_empty() {
        let retro = Retrospective::parse(REVIEW_CLEAN);
        assert!(retro.unmet_criteria.is_empty());
        assert!(retro.lessons.is_empty());
        assert!(retro.notes.is_empty());
        assert!(!retro.has_findings());
    }

    #[test]
    fn parse_is_lenient_on_garbage() {
        // A reply in the wrong shape degrades to no findings, never an error.
        let retro = Retrospective::parse("sorry, I cannot do that");
        assert!(!retro.has_findings());
        assert!(retro.lessons.is_empty());
    }

    #[tokio::test]
    async fn an_uncovered_criterion_is_flagged_and_a_lesson_is_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("brief.md"), BRIEF).unwrap();
        std::fs::write(root.join("PROGRESS.md"), DONE_PLAN).unwrap();
        // A sentinel source file that the retrospective must not touch.
        std::fs::write(root.join("src.rs"), "fn main() {}").unwrap();

        let provider = FakeProvider::new().text(REVIEW_WITH_GAP);
        let retro = run_and_record(&provider, "m", root)
            .await
            .unwrap()
            .expect("a retrospective for a completed plan");

        // The unmet criterion is flagged and the lesson is recorded.
        assert!(retro.has_findings());
        let lessons = std::fs::read_to_string(root.join("LESSONS.md")).unwrap();
        assert!(lessons.contains("Wire the user's name through"));
        assert!(lessons.starts_with("# Lessons: greeting"));
        // The only file the retrospective wrote is LESSONS.md: the sentinel source
        // is byte-for-byte unchanged.
        assert_eq!(
            std::fs::read_to_string(root.join("src.rs")).unwrap(),
            "fn main() {}"
        );
    }

    #[tokio::test]
    async fn a_fully_covered_run_flags_nothing_and_writes_no_lessons() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("brief.md"), BRIEF).unwrap();
        std::fs::write(root.join("PROGRESS.md"), DONE_PLAN).unwrap();

        let provider = FakeProvider::new().text(REVIEW_CLEAN);
        let retro = run_and_record(&provider, "m", root).await.unwrap().unwrap();

        assert!(!retro.has_findings());
        // No lessons -> no LESSONS.md is written.
        assert!(!root.join("LESSONS.md").exists());
    }

    #[tokio::test]
    async fn no_brief_means_nothing_to_review() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("PROGRESS.md"), DONE_PLAN).unwrap();
        let provider = FakeProvider::new().text(REVIEW_WITH_GAP);
        // No brief.md: skip cleanly, and the provider is never called.
        assert!(run_and_record(&provider, "m", root)
            .await
            .unwrap()
            .is_none());
        assert!(provider.requests().is_empty());
    }

    #[tokio::test]
    async fn a_plan_with_no_completed_step_is_not_reviewed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("brief.md"), BRIEF).unwrap();
        let unstarted = "# Progress: greeting\nBranch: feature/greeting\n\n## Steps\n\n- [ ] 1. Print a greeting\n";
        std::fs::write(root.join("PROGRESS.md"), unstarted).unwrap();
        let provider = FakeProvider::new().text(REVIEW_WITH_GAP);
        assert!(run_and_record(&provider, "m", root)
            .await
            .unwrap()
            .is_none());
        assert!(provider.requests().is_empty());
    }
}
