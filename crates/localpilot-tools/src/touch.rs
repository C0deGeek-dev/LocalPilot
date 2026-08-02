//! What a tool touched, reported as data rather than inferred afterwards.
//!
//! When several agents share one working tree, each needs to know when another
//! has changed a file it is working on. Getting that right hinges entirely on
//! *where the fact comes from*, and there are three tempting sources that do not
//! work:
//!
//! - **Inferring from the tool name and arguments.** Fine for `write_file`,
//!   useless for `multi_edit` (one call, many ranges, some of which may not
//!   apply) and for anything that decides what to change while it runs.
//! - **Parsing the range back out of the tool's prose output.** It reads like it
//!   works and then silently stops when the wording changes.
//! - **Watching the filesystem.** Catches everything, attributes nothing.
//!
//! So a tool *reports* what it touched, as a typed value on its own output. The
//! tool is the only thing that knows, and it knows exactly. Nothing has to be
//! parsed, and a tool that reports nothing is visibly reporting nothing rather
//! than being mistaken for one that changed nothing.
//!
//! Tools do not know about the swarm, the index, or the notification path. They
//! attach a value; the dispatch layer forwards it. Adding a mutating tool that
//! forgets to report is the one failure mode left, and it is a visibly empty
//! field rather than a silent gap in a match arm.

use std::path::{Path, PathBuf};

/// What a tool did to a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchOp {
    /// Read it. Recorded so a *reader* can be told its ground moved.
    Read,
    /// Changed existing content.
    Modified,
    /// Wrote it whole — created it, or replaced everything in it.
    Wrote,
    /// Removed it.
    Deleted,
}

impl TouchOp {
    /// Whether this changed the file. Reads are tracked, but only a change
    /// starts a conversation.
    #[must_use]
    pub fn is_mutation(self) -> bool {
        !matches!(self, TouchOp::Read)
    }

    /// A word for a notification.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            TouchOp::Read => "read",
            TouchOp::Modified => "changed",
            TouchOp::Wrote => "rewrote",
            TouchOp::Deleted => "deleted",
        }
    }
}

/// A range of lines, 1-based and inclusive.
///
/// Structured, never prose. Two agents editing the same file in places that do
/// not overlap is ordinary and should not interrupt anyone; the only way to tell
/// that apart from a real collision is to have the numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRange {
    /// First line touched.
    pub start: u32,
    /// Last line touched.
    pub end: u32,
}

impl LineRange {
    /// A range, normalised so `start <= end`.
    #[must_use]
    pub fn new(start: u32, end: u32) -> Self {
        Self {
            start: start.min(end),
            end: start.max(end),
        }
    }

    /// A single line.
    #[must_use]
    pub fn line(at: u32) -> Self {
        Self { start: at, end: at }
    }

    /// Whether two ranges share any line.
    #[must_use]
    pub fn overlaps(self, other: Self) -> bool {
        self.start <= other.end && other.start <= self.end
    }
}

impl std::fmt::Display for LineRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.start == self.end {
            write!(f, "line {}", self.start)
        } else {
            write!(f, "lines {}-{}", self.start, self.end)
        }
    }
}

/// One file, touched once, by one tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTouch {
    /// The file, as the tool resolved it.
    pub path: PathBuf,
    /// What happened to it.
    pub op: TouchOp,
    /// Which lines, when the tool knows. `None` means the whole file — a fresh
    /// write, a delete, or a change whose extent the tool genuinely cannot
    /// bound. It is deliberately *not* a stand-in for "did not bother": a whole
    /// file conflicts with everything, so guessing `None` costs noise and
    /// guessing a range costs a missed collision.
    pub lines: Option<LineRange>,
}

impl FileTouch {
    /// A touch covering a whole file.
    #[must_use]
    pub fn whole(path: impl Into<PathBuf>, op: TouchOp) -> Self {
        Self {
            path: path.into(),
            op,
            lines: None,
        }
    }

    /// A touch covering a known range.
    #[must_use]
    pub fn ranged(path: impl Into<PathBuf>, op: TouchOp, lines: LineRange) -> Self {
        Self {
            path: path.into(),
            op,
            lines: Some(lines),
        }
    }

    /// Whether this touch and `other` could be in each other's way: the same
    /// file, and either overlapping ranges or at least one whole-file touch.
    #[must_use]
    pub fn collides_with(&self, other: &FileTouch) -> bool {
        if !same_file(&self.path, &other.path) {
            return false;
        }
        match (self.lines, other.lines) {
            (Some(mine), Some(theirs)) => mine.overlaps(theirs),
            // A whole-file touch is in the way of everything in that file.
            _ => true,
        }
    }
}

/// Whether two paths name the same file.
///
/// Compared after normalisation rather than byte-wise, because the same file
/// reaches this from different tools as `src/lib.rs`, `./src/lib.rs`, and an
/// absolute path — and on Windows with either separator. Getting this wrong
/// fails *open*: two agents editing one file are told nothing.
#[must_use]
pub fn same_file(a: &Path, b: &Path) -> bool {
    normalise(a) == normalise(b)
}

/// A comparable form of a path: separators unified, `.` segments dropped, `..`
/// resolved where it can be, and case folded on the platforms whose filesystems
/// fold it.
#[must_use]
pub fn normalise(path: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for part in path.to_string_lossy().split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other.to_string()),
        }
    }
    let joined = parts.join("/");
    if cfg!(windows) || cfg!(target_os = "macos") {
        joined.to_lowercase()
    } else {
        joined
    }
}

/// The span of lines that differ between two versions of a file.
///
/// Computed from the content rather than from what the tool meant to do, which
/// is both more accurate and uniform: `edit_file`, `multi_edit`, `apply_patch`,
/// and `replace_in_file` all end up rewriting a string, and all of them get a
/// real range from this without threading byte offsets through their internals.
///
/// Scattered edits in one file collapse into the single enclosing range. That
/// over-approximates, and deliberately so: for an advisory alert, a false
/// "you two are near each other" costs a message, and a false "you are not"
/// costs the collision the whole mechanism exists to catch.
///
/// Returns `None` when the content is identical — a tool that changed nothing
/// touched nothing.
#[must_use]
pub fn changed_range(before: &str, after: &str) -> Option<LineRange> {
    if before == after {
        return None;
    }
    let old: Vec<&str> = before.lines().collect();
    let new: Vec<&str> = after.lines().collect();

    let mut first = 0usize;
    while first < old.len() && first < new.len() && old[first] == new[first] {
        first += 1;
    }
    // Walk in from the end, but never back past the common prefix, or an
    // insertion into a run of identical lines would produce an inverted range.
    let mut tail = 0usize;
    while tail < old.len().saturating_sub(first)
        && tail < new.len().saturating_sub(first)
        && old[old.len() - 1 - tail] == new[new.len() - 1 - tail]
    {
        tail += 1;
    }

    let start = first + 1;
    // The last changed line in the *new* content; for a pure deletion the new
    // content has nothing there, so the range collapses onto the join point.
    let end = new.len().saturating_sub(tail).max(first);
    Some(LineRange::new(
        u32::try_from(start).unwrap_or(u32::MAX),
        u32::try_from(end.max(start)).unwrap_or(u32::MAX),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_is_normalised_whichever_way_round_it_arrives() {
        assert_eq!(LineRange::new(10, 4), LineRange::new(4, 10));
        assert_eq!(LineRange::line(7).to_string(), "line 7");
        assert_eq!(LineRange::new(4, 10).to_string(), "lines 4-10");
    }

    #[test]
    fn ranges_overlap_only_when_they_share_a_line() {
        assert!(LineRange::new(1, 10).overlaps(LineRange::new(10, 20)));
        assert!(LineRange::new(1, 10).overlaps(LineRange::new(4, 6)));
        assert!(!LineRange::new(1, 10).overlaps(LineRange::new(11, 20)));
    }

    #[test]
    fn two_edits_far_apart_in_one_file_are_not_a_collision() {
        let mine = FileTouch::ranged("src/lib.rs", TouchOp::Modified, LineRange::new(1, 20));
        let theirs = FileTouch::ranged("src/lib.rs", TouchOp::Modified, LineRange::new(400, 420));
        assert!(!mine.collides_with(&theirs));
        assert!(!theirs.collides_with(&mine));
    }

    #[test]
    fn overlapping_edits_in_one_file_are_a_collision() {
        let mine = FileTouch::ranged("src/lib.rs", TouchOp::Modified, LineRange::new(10, 30));
        let theirs = FileTouch::ranged("src/lib.rs", TouchOp::Modified, LineRange::new(25, 40));
        assert!(mine.collides_with(&theirs));
    }

    #[test]
    fn a_whole_file_touch_collides_with_everything_in_that_file() {
        let rewrite = FileTouch::whole("src/lib.rs", TouchOp::Wrote);
        let edit = FileTouch::ranged("src/lib.rs", TouchOp::Modified, LineRange::new(400, 420));
        assert!(rewrite.collides_with(&edit));
        assert!(edit.collides_with(&rewrite));
    }

    #[test]
    fn different_files_never_collide() {
        let a = FileTouch::whole("src/a.rs", TouchOp::Wrote);
        let b = FileTouch::whole("src/b.rs", TouchOp::Wrote);
        assert!(!a.collides_with(&b));
    }

    #[test]
    fn the_same_file_reached_by_different_spellings_is_the_same_file() {
        // The failure this prevents is silent: two agents editing one file, each
        // told nothing, because one tool wrote `./src/lib.rs`.
        assert!(same_file(
            Path::new("src/lib.rs"),
            Path::new("./src/lib.rs")
        ));
        assert!(same_file(
            Path::new("src/lib.rs"),
            Path::new("src/other/../lib.rs")
        ));
        assert!(same_file(Path::new("src/lib.rs"), Path::new("src\\lib.rs"),));
        assert!(!same_file(
            Path::new("src/lib.rs"),
            Path::new("src/main.rs")
        ));
    }

    #[test]
    #[cfg(windows)]
    fn windows_paths_compare_case_insensitively() {
        assert!(same_file(Path::new("Src/Lib.rs"), Path::new("src/lib.rs")));
    }

    #[test]
    fn an_unchanged_file_has_no_changed_range() {
        assert_eq!(
            changed_range(
                "a
b
", "a
b
"
            ),
            None
        );
    }

    #[test]
    fn a_single_changed_line_is_that_line() {
        assert_eq!(
            changed_range(
                "a
b
c
", "a
B
c
"
            ),
            Some(LineRange::line(2))
        );
    }

    #[test]
    fn a_changed_block_spans_it() {
        assert_eq!(
            changed_range(
                "a
b
c
d
", "a
X
Y
d
"
            ),
            Some(LineRange::new(2, 3))
        );
    }

    #[test]
    fn an_insertion_reports_the_inserted_lines() {
        assert_eq!(
            changed_range(
                "a
d
", "a
b
c
d
"
            ),
            Some(LineRange::new(2, 3))
        );
    }

    #[test]
    fn an_insertion_among_identical_lines_does_not_invert_the_range() {
        // The end-walk must not run back past the common prefix.
        let range = changed_range(
            "x
x
", "x
x
x
",
        )
        .expect("something changed");
        assert!(range.start <= range.end, "{range}");
    }

    #[test]
    fn a_deletion_collapses_onto_the_join() {
        let range = changed_range(
            "a
b
c
", "a
c
",
        )
        .expect("something changed");
        assert!(range.start <= range.end, "{range}");
        assert_eq!(range.start, 2);
    }

    #[test]
    fn scattered_edits_collapse_into_the_enclosing_range() {
        // Over-approximating is the safe direction: a spurious alert costs a
        // message, a missed one costs the collision.
        let before = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join(
                "
",
            );
        let mut after: Vec<String> = before.lines().map(ToString::to_string).collect();
        after[1] = "CHANGED".into();
        after[7] = "CHANGED".into();
        let range = changed_range(
            &before,
            &after.join(
                "
",
            ),
        )
        .expect("something changed");
        assert_eq!(range, LineRange::new(2, 8));
    }

    #[test]
    fn only_a_change_counts_as_a_mutation() {
        assert!(!TouchOp::Read.is_mutation());
        assert!(TouchOp::Modified.is_mutation());
        assert!(TouchOp::Wrote.is_mutation());
        assert!(TouchOp::Deleted.is_mutation());
    }
}
