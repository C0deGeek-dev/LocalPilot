//! `DECISIONS.md` parsing and rendering.
//!
//! An append-only log of deviations the loop makes from `brief.md` / `PROGRESS.md`
//! during a run — most notably a replan after the attempt budget is spent. Like
//! the brief and progress documents it is authoritative and user-editable, so the
//! model is the renderer rather than the source of truth: the next run reads the
//! edited file. Parsing and rendering round-trip so an appended entry never
//! reshuffles the entries already written.

use crate::error::HarnessError;

const DOCUMENT: &str = "DECISIONS.md";

/// A parsed `DECISIONS.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decisions {
    /// The run name, from the `# Decisions: <name>` title.
    pub name: String,
    /// The entries, oldest first.
    pub entries: Vec<Decision>,
}

/// One recorded deviation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// Stable id, `D001`-style, assigned on append.
    pub id: String,
    /// The date the entry was recorded (`YYYY-MM-DD`).
    pub date: String,
    /// A one-line title.
    pub title: String,
    /// What changed.
    pub decision: String,
    /// Why it changed.
    pub rationale: String,
    /// The step number(s) or files the decision touches.
    pub refs: String,
}

const SEPARATOR: &str = " · ";

impl Decisions {
    /// An empty log for `name`.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            entries: Vec::new(),
        }
    }

    /// Parse a decisions log from markdown text.
    ///
    /// # Errors
    /// Returns [`HarnessError::Malformed`] if the `# Decisions: <name>` title is
    /// missing.
    pub fn parse(text: &str) -> Result<Self, HarnessError> {
        let text = text.replace("\r\n", "\n");
        let name = crate::brief::title_after(&text, "# Decisions:").ok_or_else(|| {
            HarnessError::Malformed {
                document: DOCUMENT,
                detail: "missing '# Decisions: <name>' title".to_string(),
            }
        })?;

        let mut entries: Vec<Decision> = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(header) = trimmed.strip_prefix("- ").filter(|h| is_entry_header(h)) {
                let mut parts = header.splitn(3, SEPARATOR);
                let id = parts.next().unwrap_or_default().trim().to_string();
                let date = parts.next().unwrap_or_default().trim().to_string();
                let title = parts.next().unwrap_or_default().trim().to_string();
                entries.push(Decision {
                    id,
                    date,
                    title,
                    decision: String::new(),
                    rationale: String::new(),
                    refs: String::new(),
                });
            } else if let Some(entry) = entries.last_mut() {
                if let Some(value) = trimmed.strip_prefix("- decision:") {
                    entry.decision = value.trim().to_string();
                } else if let Some(value) = trimmed.strip_prefix("- rationale:") {
                    entry.rationale = value.trim().to_string();
                } else if let Some(value) = trimmed.strip_prefix("- refs:") {
                    entry.refs = value.trim().to_string();
                }
            }
        }

        Ok(Self { name, entries })
    }

    /// Append an entry, assigning it the next free `D###` id, and return that id.
    pub fn append(
        &mut self,
        date: impl Into<String>,
        title: impl Into<String>,
        decision: impl Into<String>,
        rationale: impl Into<String>,
        refs: impl Into<String>,
    ) -> String {
        let id = self.next_id();
        self.entries.push(Decision {
            id: id.clone(),
            date: date.into(),
            title: title.into(),
            decision: decision.into(),
            rationale: rationale.into(),
            refs: refs.into(),
        });
        id
    }

    /// The next free `D###` id: one past the highest numeric id already present.
    fn next_id(&self) -> String {
        let highest = self
            .entries
            .iter()
            .filter_map(|entry| entry.id.strip_prefix('D'))
            .filter_map(|n| n.parse::<u32>().ok())
            .max()
            .unwrap_or(0);
        format!("D{:03}", highest + 1)
    }

    /// Render the log back to markdown. Round-trips through [`Decisions::parse`].
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!("# Decisions: {}\n\n", self.name);
        for entry in &self.entries {
            out.push_str(&format!(
                "- {}{SEPARATOR}{}{SEPARATOR}{}\n",
                entry.id, entry.date, entry.title
            ));
            out.push_str(&format!("  - decision: {}\n", entry.decision));
            out.push_str(&format!("  - rationale: {}\n", entry.rationale));
            out.push_str(&format!("  - refs: {}\n\n", entry.refs));
        }
        out
    }
}

/// Whether a `- ` bullet body is an entry header (`D### · date · title`) rather
/// than one of the `decision:` / `rationale:` / `refs:` field bullets.
fn is_entry_header(body: &str) -> bool {
    body.starts_with('D') && body.contains(SEPARATOR)
}

/// Today's date as `YYYY-MM-DD` (UTC), for stamping a new entry. Falls back to
/// the epoch date if the system clock predates it.
#[must_use]
pub fn today() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    format!("{year:04}-{month:02}-{day:02}")
}

/// Convert a count of days since the Unix epoch (1970-01-01) to a civil
/// `(year, month, day)` in the proleptic Gregorian calendar. This is the
/// standard days→civil conversion, implemented directly so the crate needs no
/// date-library dependency.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Shift the epoch to 0000-03-01 so leap days fall at the end of each cycle.
    let shifted = days + 719_468;
    let adjusted = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    };
    let era = adjusted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365; // [0, 399]
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100); // [0, 365]
    let month_pos = (5 * day_of_year + 2) / 153; // [0, 11]
    let day = (day_of_year - (153 * month_pos + 2) / 5 + 1) as u32; // [1, 31]
    let month = if month_pos < 10 {
        month_pos + 3
    } else {
        month_pos - 9
    } as u32; // [1, 12]
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# Decisions: greeting\n\n\
- D001 · 2026-06-04 · Replan step 1\n\
\u{20}\u{20}- decision: the attempt budget was exhausted; the step is queued for replanning\n\
\u{20}\u{20}- rationale: attempts failed: lint failed; lint failed\n\
\u{20}\u{20}- refs: step 1\n\n";

    #[test]
    fn parses_a_sample_log() {
        let decisions = Decisions::parse(SAMPLE).unwrap();
        assert_eq!(decisions.name, "greeting");
        assert_eq!(decisions.entries.len(), 1);
        let entry = &decisions.entries[0];
        assert_eq!(entry.id, "D001");
        assert_eq!(entry.date, "2026-06-04");
        assert_eq!(entry.title, "Replan step 1");
        assert_eq!(entry.refs, "step 1");
        assert!(entry.rationale.contains("lint failed"));
    }

    #[test]
    fn rejects_a_log_missing_its_title() {
        let err = Decisions::parse("- D001 · x · y\n").unwrap_err();
        assert!(matches!(err, HarnessError::Malformed { .. }));
    }

    #[test]
    fn render_round_trips_through_parse() {
        let decisions = Decisions::parse(SAMPLE).unwrap();
        let reparsed = Decisions::parse(&decisions.render()).unwrap();
        assert_eq!(decisions, reparsed);
    }

    #[test]
    fn append_assigns_sequential_ids_and_round_trips() {
        let mut decisions = Decisions::new("greeting");
        let first = decisions.append(
            "2026-06-04",
            "Replan step 1",
            "queued for replanning",
            "attempts failed",
            "step 1",
        );
        let second = decisions.append(
            "2026-06-04",
            "Replan step 2",
            "queued for replanning",
            "attempts failed",
            "step 2",
        );
        assert_eq!(first, "D001");
        assert_eq!(second, "D002");
        let reparsed = Decisions::parse(&decisions.render()).unwrap();
        assert_eq!(decisions, reparsed);
    }

    #[test]
    fn append_continues_numbering_after_a_parsed_log() {
        let mut decisions = Decisions::parse(SAMPLE).unwrap();
        let id = decisions.append("2026-06-04", "t", "d", "r", "refs");
        assert_eq!(id, "D002");
    }

    #[test]
    fn accepts_crlf_line_endings() {
        let crlf = SAMPLE.replace('\n', "\r\n");
        assert_eq!(Decisions::parse(&crlf).unwrap().entries.len(), 1);
    }

    #[test]
    fn civil_date_matches_known_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        // 1970 is not a leap year: day index 59 is 1 March.
        assert_eq!(civil_from_days(59), (1970, 3, 1));
        // 1972 is a leap year: day index 789 is its 29 February.
        assert_eq!(civil_from_days(789), (1972, 2, 29));
    }

    #[test]
    fn today_is_iso_shaped() {
        let today = today();
        assert_eq!(today.len(), 10);
        assert_eq!(today.match_indices('-').count(), 2);
    }
}
