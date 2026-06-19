//! `LESSONS.md` parsing and rendering.
//!
//! An append-only log of lessons the completion retrospective captures at the end
//! of a run. Like `brief.md` / `PROGRESS.md` / `DECISIONS.md` it is authoritative
//! and user-editable and sited at the project root; the model is the renderer, not
//! the source of truth, so the next run reads the edited file. Parsing and
//! rendering round-trip, so an appended lesson never reshuffles the lessons
//! already written.

use crate::brief::title_after;
use crate::error::HarnessError;

const DOCUMENT: &str = "LESSONS.md";
const SEPARATOR: &str = " · ";

/// A parsed `LESSONS.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lessons {
    /// The run name, from the `# Lessons: <name>` title.
    pub name: String,
    /// The entries, oldest first.
    pub entries: Vec<Lesson>,
}

/// One recorded lesson.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lesson {
    /// The date the lesson was recorded (`YYYY-MM-DD`).
    pub date: String,
    /// The lesson text, a single line.
    pub text: String,
}

impl Lessons {
    /// An empty log for `name`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            entries: Vec::new(),
        }
    }

    /// Parse a lessons log from markdown text.
    ///
    /// # Errors
    /// Returns [`HarnessError::Malformed`] if the `# Lessons: <name>` title is
    /// missing.
    pub fn parse(text: &str) -> Result<Self, HarnessError> {
        let text = text.replace("\r\n", "\n");
        let name = title_after(&text, "# Lessons:").ok_or_else(|| HarnessError::Malformed {
            document: DOCUMENT,
            detail: "missing '# Lessons: <name>' title".to_string(),
        })?;

        let mut entries: Vec<Lesson> = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(body) = trimmed.strip_prefix("- ") {
                if let Some((date, lesson)) = body.split_once(SEPARATOR) {
                    entries.push(Lesson {
                        date: date.trim().to_string(),
                        text: lesson.trim().to_string(),
                    });
                }
            }
        }

        Ok(Self { name, entries })
    }

    /// Append a lesson recorded on `date`.
    pub fn append(&mut self, date: impl Into<String>, text: impl Into<String>) {
        self.entries.push(Lesson {
            date: date.into(),
            text: text.into(),
        });
    }

    /// Render the log back to markdown. Round-trips through [`Lessons::parse`].
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!("# Lessons: {}\n\n", self.name);
        for entry in &self.entries {
            out.push_str(&format!("- {}{SEPARATOR}{}\n", entry.date, entry.text));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# Lessons: greeting\n\n\
- 2026-06-04 · Prefer extending the existing store over a parallel cache\n\
- 2026-06-05 · A coverage test must name the behaviour it pins\n";

    #[test]
    fn parses_a_sample_log() {
        let lessons = Lessons::parse(SAMPLE).unwrap();
        assert_eq!(lessons.name, "greeting");
        assert_eq!(lessons.entries.len(), 2);
        assert_eq!(lessons.entries[0].date, "2026-06-04");
        assert!(lessons.entries[0].text.contains("existing store"));
    }

    #[test]
    fn rejects_a_log_missing_its_title() {
        let err = Lessons::parse("- 2026-06-04 · a lesson\n").unwrap_err();
        assert!(matches!(err, HarnessError::Malformed { .. }));
    }

    #[test]
    fn append_and_render_round_trips_through_parse() {
        let mut lessons = Lessons::new("greeting");
        lessons.append(
            "2026-06-04",
            "Prefer extending the existing store over a parallel cache",
        );
        lessons.append(
            "2026-06-05",
            "A coverage test must name the behaviour it pins",
        );
        let reparsed = Lessons::parse(&lessons.render()).unwrap();
        assert_eq!(lessons, reparsed);
    }

    #[test]
    fn appending_to_a_parsed_log_keeps_existing_entries() {
        let mut lessons = Lessons::parse(SAMPLE).unwrap();
        lessons.append("2026-06-06", "A third lesson");
        assert_eq!(lessons.entries.len(), 3);
        // The pre-existing entries are untouched and still first.
        assert_eq!(lessons.entries[0].date, "2026-06-04");
        let reparsed = Lessons::parse(&lessons.render()).unwrap();
        assert_eq!(lessons, reparsed);
    }

    #[test]
    fn accepts_crlf_line_endings() {
        let crlf = SAMPLE.replace('\n', "\r\n");
        assert_eq!(Lessons::parse(&crlf).unwrap().entries.len(), 2);
    }
}
