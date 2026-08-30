//! Shared text-shaping helpers for lean, one-line summaries.
//!
//! Several pull/search surfaces condense a longer description into a single
//! bounded line for a locator or index preview: skill search, the tool broker,
//! the layered memory pack, the harness handoff header, and the skills CLI.
//! They all start by collapsing internal whitespace; most then cap to a
//! character budget. Keeping the shaping here means one definition, one set of
//! Unicode-safe truncation rules, and one place to test the edge cases.

/// Default character budget for a one-line locator/preview summary.
pub const SUMMARY_CHARS: usize = 100;

/// Collapse every run of ASCII/Unicode whitespace to a single space and trim
/// the ends — the shared first step of every one-line summary. No length cap.
#[must_use]
pub fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Collapse whitespace and cap the result to `max` characters, appending an
/// ellipsis (`…`) when the text was actually truncated. The canonical lean
/// summary used by locator surfaces that show a visible "there's more" marker.
///
/// Truncation is by `char`, so it never splits a multi-byte code point.
#[must_use]
pub fn one_line(text: &str, max: usize) -> String {
    let collapsed = collapse_whitespace(text);
    let mut shown: String = collapsed.chars().take(max).collect();
    if collapsed.chars().count() > max {
        shown.push('…');
    }
    shown
}

/// Collapse whitespace and hard-truncate to `max` characters with **no**
/// ellipsis marker. Used where the cap is a silent storage/preview bound rather
/// than a visible "more" affordance (e.g. the layered index summary).
///
/// Truncation is by `char`, so it never splits a multi-byte code point.
#[must_use]
pub fn truncate_collapsed(text: &str, max: usize) -> String {
    collapse_whitespace(text).chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_flattens_runs_of_whitespace() {
        assert_eq!(collapse_whitespace("  a \n b\t c  "), "a b c");
        assert_eq!(collapse_whitespace(""), "");
    }

    #[test]
    fn one_line_appends_ellipsis_only_when_truncated() {
        assert_eq!(one_line("short", 100), "short");
        assert_eq!(one_line("abcdef", 3), "abc…");
        // Exactly at the boundary: no ellipsis.
        assert_eq!(one_line("abc", 3), "abc");
    }

    #[test]
    fn one_line_truncates_on_char_boundaries() {
        // Three multi-byte chars capped to two must not panic or split a char.
        assert_eq!(one_line("héllo", 2), "hé…");
    }

    #[test]
    fn truncate_collapsed_never_adds_a_marker() {
        assert_eq!(truncate_collapsed("a  b  c", 3), "a b");
        assert_eq!(truncate_collapsed("abc", 10), "abc");
    }
}
