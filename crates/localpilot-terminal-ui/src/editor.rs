use unicode_segmentation::UnicodeSegmentation;

use crate::sanitize_text;
use crate::text::{byte_at_display_column, display_width, wrap_ranges};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorRow {
    pub start_byte: usize,
    pub end_byte: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Editor {
    text: String,
    cursor: usize,
    preferred_column: Option<usize>,
    history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
    history_draft_cursor: usize,
}

impl Editor {
    #[must_use]
    pub fn new(history: Vec<String>) -> Self {
        Self {
            history,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// Replace the recall source without disturbing the current draft. Hosts
    /// call this after the first frame when durable history has been loaded.
    pub fn seed_history(&mut self, history: Vec<String>) {
        self.history = history;
        self.history_index = None;
        self.history_draft.clear();
        self.history_draft_cursor = 0;
        self.preferred_column = None;
    }

    #[must_use]
    pub fn history_match_reverse(
        &self,
        query: &str,
        before_exclusive: Option<usize>,
    ) -> Option<(usize, String)> {
        let query = query.to_lowercase();
        let end = before_exclusive
            .unwrap_or(self.history.len())
            .min(self.history.len());
        self.history[..end]
            .iter()
            .enumerate()
            .rev()
            .find(|(_, entry)| entry.to_lowercase().contains(&query))
            .map(|(index, entry)| (index, entry.clone()))
    }

    #[must_use]
    pub fn history_match_forward(
        &self,
        query: &str,
        after_exclusive: usize,
    ) -> Option<(usize, String)> {
        let query = query.to_lowercase();
        self.history
            .iter()
            .enumerate()
            .skip(after_exclusive.saturating_add(1))
            .find(|(_, entry)| entry.to_lowercase().contains(&query))
            .map(|(index, entry)| (index, entry.clone()))
    }

    pub fn replace_draft(&mut self, text: impl Into<String>) {
        let text = text.into();
        let cursor = text.len();
        self.replace_draft_at(text, cursor);
    }

    pub fn replace_draft_at(&mut self, text: impl Into<String>, cursor: usize) {
        self.text = sanitize_text(&text.into());
        self.cursor = cursor.min(self.text.len());
        self.normalize_cursor();
        self.preferred_column = None;
        self.history_index = None;
        self.history_draft.clear();
        self.history_draft_cursor = 0;
    }

    #[must_use]
    pub fn visual_rows(&self, width: u16) -> Vec<EditorRow> {
        wrap_ranges(&self.text, width)
            .into_iter()
            .map(|row| EditorRow {
                start_byte: row.start_byte,
                end_byte: row.end_byte,
            })
            .collect()
    }

    #[must_use]
    pub fn cursor_row_and_column(&self, width: u16) -> (usize, u16) {
        text_row_and_column(&self.text, self.cursor, width)
    }

    pub fn set_cursor_from_visual(&mut self, row_index: usize, column: u16, width: u16) {
        let rows = self.visual_rows(width);
        if let Some(row) = rows.get(row_index) {
            self.cursor = byte_at_display_column(
                &self.text,
                row.start_byte,
                row.end_byte,
                usize::from(column),
            );
            self.preferred_column = None;
        }
    }

    pub fn insert(&mut self, text: &str) {
        self.fork_recall_on_edit();
        self.normalize_cursor();
        self.preferred_column = None;
        let text = sanitize_text(text);
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }

    pub fn backspace(&mut self) {
        self.fork_recall_on_edit();
        self.normalize_cursor();
        self.preferred_column = None;
        if let Some((byte, _)) = self.text[..self.cursor].grapheme_indices(true).next_back() {
            self.text.drain(byte..self.cursor);
            self.cursor = byte;
        }
    }

    pub fn delete(&mut self) {
        self.fork_recall_on_edit();
        self.normalize_cursor();
        self.preferred_column = None;
        if let Some(grapheme) = self.text[self.cursor..].graphemes(true).next() {
            self.text.drain(self.cursor..self.cursor + grapheme.len());
        }
    }

    pub fn move_left(&mut self) {
        self.normalize_cursor();
        self.preferred_column = None;
        if let Some((byte, _)) = self.text[..self.cursor].grapheme_indices(true).next_back() {
            self.cursor = byte;
        }
    }

    pub fn move_right(&mut self) {
        self.normalize_cursor();
        self.preferred_column = None;
        if let Some(grapheme) = self.text[self.cursor..].graphemes(true).next() {
            self.cursor += grapheme.len();
        }
    }

    pub fn up_or_history(&mut self, width: u16) {
        let rows = self.visual_rows(width);
        let row_index = cursor_row(&rows, self.cursor);
        if row_index == 0 {
            self.recall_previous();
        } else {
            self.move_to_adjacent_row(&rows, row_index, row_index - 1);
        }
    }

    pub fn down_or_history(&mut self, width: u16) {
        let rows = self.visual_rows(width);
        let row_index = cursor_row(&rows, self.cursor);
        if row_index + 1 >= rows.len() {
            self.recall_next();
        } else {
            self.move_to_adjacent_row(&rows, row_index, row_index + 1);
        }
    }

    pub fn submit(&mut self) -> Option<String> {
        if self.text.trim().is_empty() {
            return None;
        }
        let submitted = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.preferred_column = None;
        self.history_index = None;
        self.history_draft.clear();
        self.history_draft_cursor = 0;
        if self.history.last() != Some(&submitted) {
            self.history.push(submitted.clone());
        }
        Some(submitted)
    }

    pub fn move_visual_start(&mut self, width: u16) {
        self.normalize_cursor();
        let rows = self.visual_rows(width);
        let row = &rows[cursor_row(&rows, self.cursor)];
        self.cursor = row.start_byte;
        self.preferred_column = None;
    }

    pub fn move_visual_end(&mut self, width: u16) {
        self.normalize_cursor();
        let rows = self.visual_rows(width);
        let row = &rows[cursor_row(&rows, self.cursor)];
        self.cursor = row.end_byte;
        self.preferred_column = None;
    }

    pub fn move_line_start(&mut self) {
        self.normalize_cursor();
        self.cursor = self.line_start();
        self.preferred_column = None;
    }

    pub fn move_line_end(&mut self) {
        self.normalize_cursor();
        self.cursor = self.line_end();
        self.preferred_column = None;
    }

    pub fn move_text_start(&mut self) {
        self.cursor = 0;
        self.preferred_column = None;
    }

    pub fn move_text_end(&mut self) {
        self.cursor = self.text.len();
        self.preferred_column = None;
    }

    pub fn move_word_left(&mut self) {
        self.normalize_cursor();
        self.cursor = previous_word_start(&self.text, self.cursor);
        self.preferred_column = None;
    }

    pub fn move_word_right(&mut self) {
        self.normalize_cursor();
        self.cursor = next_word_end(&self.text, self.cursor);
        self.preferred_column = None;
    }

    pub fn delete_word_left(&mut self) {
        self.fork_recall_on_edit();
        self.normalize_cursor();
        self.preferred_column = None;
        let start = previous_word_start(&self.text, self.cursor);
        self.text.drain(start..self.cursor);
        self.cursor = start;
    }

    pub fn delete_to_line_start(&mut self) {
        self.fork_recall_on_edit();
        self.normalize_cursor();
        self.preferred_column = None;
        let start = self.line_start();
        self.text.drain(start..self.cursor);
        self.cursor = start;
    }

    pub fn delete_to_line_end(&mut self) {
        self.fork_recall_on_edit();
        self.normalize_cursor();
        self.preferred_column = None;
        let end = self.line_end();
        let delete_end = if self.cursor == end && end < self.text.len() {
            end + self.text[end..].graphemes(true).next().map_or(0, str::len)
        } else {
            end
        };
        self.text.drain(self.cursor..delete_end);
    }

    fn move_to_adjacent_row(&mut self, rows: &[EditorRow], from: usize, to: usize) {
        let source = &rows[from];
        let column = *self.preferred_column.get_or_insert_with(|| {
            display_width(&self.text[source.start_byte..self.cursor.min(source.end_byte)])
        });
        let target = &rows[to];
        self.cursor =
            byte_at_display_column(&self.text, target.start_byte, target.end_byte, column);
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |offset| offset + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |offset| self.cursor + offset)
    }

    fn recall_previous(&mut self) {
        self.preferred_column = None;
        if self.history.is_empty() {
            return;
        }
        let index = match self.history_index {
            Some(index) => index.saturating_sub(1),
            None => {
                self.history_draft = self.text.clone();
                self.history_draft_cursor = self.cursor;
                self.history.len() - 1
            }
        };
        self.text = self.history[index].clone();
        self.cursor = self.text.len();
        self.history_index = Some(index);
    }

    fn recall_next(&mut self) {
        self.preferred_column = None;
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 < self.history.len() {
            self.text = self.history[index + 1].clone();
            self.cursor = self.text.len();
            self.history_index = Some(index + 1);
        } else {
            self.text = std::mem::take(&mut self.history_draft);
            self.cursor = self.history_draft_cursor.min(self.text.len());
            self.history_draft_cursor = 0;
            self.history_index = None;
        }
    }

    fn fork_recall_on_edit(&mut self) {
        if self.history_index.take().is_some() {
            self.history_draft.clear();
            self.history_draft_cursor = 0;
        }
    }

    fn normalize_cursor(&mut self) {
        self.cursor = self.cursor.min(self.text.len());
        while !self.text.is_char_boundary(self.cursor) {
            self.cursor = self.cursor.saturating_sub(1);
        }
    }
}

pub(crate) fn text_row_and_column(text: &str, cursor: usize, width: u16) -> (usize, u16) {
    let rows: Vec<EditorRow> = wrap_ranges(text, width)
        .into_iter()
        .map(|row| EditorRow {
            start_byte: row.start_byte,
            end_byte: row.end_byte,
        })
        .collect();
    let cursor = cursor.min(text.len());
    let row_index = cursor_row(&rows, cursor);
    let row = &rows[row_index];
    let slice = &text[row.start_byte..cursor.min(row.end_byte)];
    let column = u16::try_from(display_width(slice)).unwrap_or(u16::MAX);
    (row_index, column)
}

fn is_word_segment(segment: &str) -> bool {
    segment.chars().any(|character| {
        character.is_alphanumeric()
            || character == '_'
            || (!character.is_ascii_punctuation() && !character.is_whitespace())
    })
}

fn previous_word_start(text: &str, cursor: usize) -> usize {
    UnicodeSegmentation::split_word_bound_indices(&text[..cursor])
        .rev()
        .find_map(|(start, segment)| is_word_segment(segment).then_some(start))
        .unwrap_or(0)
}

fn next_word_end(text: &str, cursor: usize) -> usize {
    UnicodeSegmentation::split_word_bound_indices(&text[cursor..])
        .find_map(|(start, segment)| {
            is_word_segment(segment).then_some(cursor + start + segment.len())
        })
        .unwrap_or(text.len())
}

fn cursor_row(rows: &[EditorRow], cursor: usize) -> usize {
    rows.iter()
        .rposition(|row| row.start_byte == cursor)
        .or_else(|| {
            rows.iter()
                .position(|row| row.start_byte < cursor && cursor <= row.end_byte)
        })
        .unwrap_or_else(|| rows.len().saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn up_down_moves_then_recalls_and_restores_full_draft_and_caret() {
        let mut editor = Editor::new(vec![
            "older prompt".to_string(),
            "newer first\nnewer second".to_string(),
        ]);
        editor.insert("draft one\ndraft two");
        editor.move_left();

        editor.up_or_history(40);
        assert_eq!(editor.text(), "draft one\ndraft two");
        let draft_cursor = editor.cursor();
        editor.up_or_history(40);
        assert_eq!(editor.text(), "newer first\nnewer second");
        editor.down_or_history(40);
        assert_eq!(editor.text(), "draft one\ndraft two");
        assert_eq!(editor.cursor(), draft_cursor);
    }

    #[test]
    fn editing_recalled_input_forks_it_from_draft_navigation() {
        let mut editor = Editor::new(vec!["remembered".to_string()]);
        editor.insert("draft");
        editor.up_or_history(80);
        editor.insert("!");
        editor.down_or_history(80);
        assert_eq!(editor.text(), "remembered!");
    }

    #[test]
    fn seeding_durable_history_preserves_an_existing_draft() {
        let mut editor = Editor::default();
        editor.insert("draft");
        editor.move_left();
        let cursor = editor.cursor();

        editor.seed_history(vec!["persisted".to_string()]);
        assert_eq!(editor.text(), "draft");
        assert_eq!(editor.cursor(), cursor);
        editor.up_or_history(80);
        assert_eq!(editor.text(), "persisted");
    }

    #[test]
    fn cursor_at_wrap_boundary_is_on_following_visual_row() {
        let mut editor = Editor::default();
        editor.insert("abcdef");
        editor.move_left();
        editor.move_left();
        editor.move_left();
        assert_eq!(editor.cursor_row_and_column(3), (1, 0));
    }

    #[test]
    fn editing_never_splits_a_wide_grapheme() {
        let mut editor = Editor::default();
        editor.insert("a界🧪z");
        editor.move_left();
        editor.move_left();
        editor.backspace();
        assert_eq!(editor.text(), "a🧪z");
        assert!(editor.text().is_char_boundary(editor.cursor()));
    }

    #[test]
    fn pasted_terminal_controls_never_enter_editor_state() {
        let mut editor = Editor::default();
        editor.insert("safe\x1b[2J text\0");
        assert_eq!(editor.text(), "safe text");
    }

    #[test]
    fn vertical_movement_keeps_the_original_display_column_across_short_rows() {
        let mut editor = Editor::default();
        editor.insert("abcdef\nxy\nabcdef");

        editor.up_or_history(80);
        assert_eq!(editor.cursor_row_and_column(80), (1, 2));
        editor.up_or_history(80);
        assert_eq!(editor.cursor_row_and_column(80), (0, 6));

        editor.down_or_history(80);
        editor.move_left();
        editor.up_or_history(80);
        assert_eq!(editor.cursor_row_and_column(80), (0, 1));
    }

    #[test]
    fn visual_line_logical_line_and_whole_text_bounds_are_distinct() {
        let mut editor = Editor::default();
        editor.insert("abcdef\nxy");
        editor.move_text_start();
        editor.move_right();
        editor.move_visual_end(3);
        assert_eq!(editor.cursor(), 3);
        editor.move_line_end();
        assert_eq!(editor.cursor(), 6);
        editor.move_text_end();
        assert_eq!(editor.cursor(), editor.text().len());
        editor.move_visual_start(3);
        assert_eq!(editor.cursor(), 7);
        editor.move_text_start();
        assert_eq!(editor.cursor(), 0);
    }

    #[test]
    fn home_and_ctrl_a_diverge_on_a_wrapped_logical_line() {
        let mut editor = Editor::default();
        editor.insert("abcdef");
        editor.move_left();
        editor.move_visual_start(3);
        assert_eq!(editor.cursor(), 3);
        editor.move_line_start();
        assert_eq!(editor.cursor(), 0);
    }

    #[test]
    fn a_mouse_position_resets_the_vertical_preferred_column() {
        let mut editor = Editor::default();
        editor.insert("abcdef\nxy\nabcdef");
        editor.up_or_history(80);
        editor.set_cursor_from_visual(1, 1, 80);
        editor.up_or_history(80);
        assert_eq!(editor.cursor_row_and_column(80), (0, 1));
    }

    #[test]
    fn word_and_kill_commands_keep_unicode_boundaries_valid() {
        let mut editor = Editor::default();
        editor.insert("alpha 界 beta");
        editor.move_word_left();
        assert_eq!(&editor.text()[editor.cursor()..], "beta");
        editor.move_word_left();
        assert_eq!(&editor.text()[editor.cursor()..], "界 beta");
        assert!(editor.text().is_char_boundary(editor.cursor()));

        editor.move_text_end();
        editor.delete_word_left();
        assert_eq!(editor.text(), "alpha 界 ");
        assert!(editor.text().is_char_boundary(editor.cursor()));

        editor.insert("🧪 beta\ngamma");
        editor.move_text_end();
        editor.delete_to_line_start();
        assert_eq!(editor.text(), "alpha 界 🧪 beta\n");
        assert!(editor.text().is_char_boundary(editor.cursor()));
    }

    #[test]
    fn ctrl_k_at_a_logical_line_end_removes_the_line_break() {
        let mut editor = Editor::default();
        editor.insert("first\nsecond");
        editor.move_text_start();
        editor.move_line_end();
        editor.delete_to_line_end();
        assert_eq!(editor.text(), "firstsecond");
        assert_eq!(editor.cursor(), 5);
    }
}
