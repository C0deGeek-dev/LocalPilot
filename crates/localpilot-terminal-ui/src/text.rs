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
