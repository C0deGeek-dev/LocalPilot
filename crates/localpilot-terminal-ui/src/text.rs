use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TextRow {
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
}

pub(crate) fn wrap_ranges(text: &str, width: u16) -> Vec<TextRow> {
    if text.is_empty() {
        return vec![TextRow {
            start_byte: 0,
            end_byte: 0,
        }];
    }

    let width = usize::from(width.max(1));
    let mut rows = Vec::new();
    let mut row_start = 0usize;
    let mut used = 0usize;
    for (byte, grapheme) in text.grapheme_indices(true) {
        if matches!(grapheme, "\n" | "\r\n") {
            rows.push(TextRow {
                start_byte: row_start,
                end_byte: byte,
            });
            row_start = byte + grapheme.len();
            used = 0;
            continue;
        }
        let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
        if used > 0 && used + grapheme_width > width {
            rows.push(TextRow {
                start_byte: row_start,
                end_byte: byte,
            });
            row_start = byte;
            used = 0;
        }
        used += grapheme_width;
    }
    rows.push(TextRow {
        start_byte: row_start,
        end_byte: text.len(),
    });
    rows
}

/// Wraps prose at word boundaries. The whitespace a row breaks on belongs to
/// neither row; a word wider than `width` is split between graphemes.
pub(crate) fn wrap_words(text: &str, width: u16) -> Vec<TextRow> {
    let width = usize::from(width.max(1));
    let mut rows = Vec::new();
    let mut line_start = 0usize;
    for (byte, grapheme) in text.grapheme_indices(true) {
        if matches!(grapheme, "\n" | "\r\n") {
            wrap_line_words(text, line_start, byte, width, &mut rows);
            line_start = byte + grapheme.len();
        }
    }
    wrap_line_words(text, line_start, text.len(), width, &mut rows);
    rows
}

fn wrap_line_words(text: &str, start: usize, end: usize, width: usize, rows: &mut Vec<TextRow>) {
    let mut words = Vec::new();
    let mut word_start = None;
    for (relative, grapheme) in text[start..end].grapheme_indices(true) {
        let byte = start + relative;
        let blank = grapheme.chars().all(char::is_whitespace);
        match (blank, word_start) {
            (true, Some(open)) => {
                words.push((open, byte));
                word_start = None;
            }
            (false, None) => word_start = Some(byte),
            _ => {}
        }
    }
    if let Some(open) = word_start {
        words.push((open, end));
    }
    if words.is_empty() {
        rows.push(TextRow {
            start_byte: start,
            end_byte: start,
        });
        return;
    }

    // `row` is the open row's start byte, end byte and display width.
    let mut row: Option<(usize, usize, usize)> = None;
    for (word_start, word_end) in words {
        let word_width = UnicodeWidthStr::width(&text[word_start..word_end]);
        if let Some((row_start, row_end, used)) = row {
            let gap = UnicodeWidthStr::width(&text[row_end..word_start]);
            if used + gap + word_width <= width {
                row = Some((row_start, word_end, used + gap + word_width));
                continue;
            }
            rows.push(TextRow {
                start_byte: row_start,
                end_byte: row_end,
            });
            row = None;
        }
        if word_width <= width {
            row = Some((word_start, word_end, word_width));
            continue;
        }
        // Too wide for any row: break between graphemes.
        for (relative, grapheme) in text[word_start..word_end].grapheme_indices(true) {
            let byte = word_start + relative;
            let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
            let next_end = byte + grapheme.len();
            row = match row {
                Some((row_start, row_end, used)) if used + grapheme_width > width => {
                    rows.push(TextRow {
                        start_byte: row_start,
                        end_byte: row_end,
                    });
                    Some((byte, next_end, grapheme_width))
                }
                Some((row_start, _, used)) => Some((row_start, next_end, used + grapheme_width)),
                None => Some((byte, next_end, grapheme_width)),
            };
        }
    }
    if let Some((row_start, row_end, _)) = row {
        rows.push(TextRow {
            start_byte: row_start,
            end_byte: row_end,
        });
    }
}

pub(crate) fn byte_at_display_column(text: &str, start: usize, end: usize, column: usize) -> usize {
    let mut used = 0usize;
    for (relative, grapheme) in text[start..end].grapheme_indices(true) {
        let width = UnicodeWidthStr::width(grapheme).max(1);
        if used + width > column {
            return start + relative;
        }
        used += width;
    }
    end
}

pub(crate) fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(text: &str, width: u16) -> Vec<&str> {
        wrap_words(text, width)
            .into_iter()
            .map(|row| &text[row.start_byte..row.end_byte])
            .collect()
    }

    #[test]
    fn words_break_on_whitespace_and_drop_the_break() {
        assert_eq!(
            rows("pick the merge picker  mode now", 10),
            ["pick the", "merge", "picker", "mode now"]
        );
        assert_eq!(rows("fits", 10), ["fits"]);
        assert_eq!(rows("", 10), [""]);
    }

    #[test]
    fn a_word_wider_than_the_row_splits_between_graphemes() {
        assert_eq!(rows("a abcdefghij b", 4), ["a", "abcd", "efgh", "ij b"]);
    }

    #[test]
    fn newlines_force_breaks_and_keep_blank_lines() {
        assert_eq!(rows("one two\n\nthree", 20), ["one two", "", "three"]);
        assert_eq!(rows("one\r\ntwo", 20), ["one", "two"]);
    }

    #[test]
    fn wide_graphemes_count_their_display_width() {
        assert_eq!(rows("日本語 テキスト", 6), ["日本語", "テキス", "ト"]);
    }
}
