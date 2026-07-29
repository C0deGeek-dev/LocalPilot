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
        let rows = self.visual_rows(width);
        let row_index = cursor_row(&rows, self.cursor);
        let row = &rows[row_index];
        let slice = &self.text[row.start_byte..self.cursor.min(row.end_byte)];
        let column = u16::try_from(display_width(slice)).unwrap_or(u16::MAX);
        (row_index, column)
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
        }
    }

    pub fn insert(&mut self, text: &str) {
        self.fork_recall_on_edit();
        self.normalize_cursor();
        let text = sanitize_text(text);
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }

    pub fn backspace(&mut self) {
        self.fork_recall_on_edit();
        self.normalize_cursor();
        if let Some((byte, _)) = self.text[..self.cursor].grapheme_indices(true).next_back() {
            self.text.drain(byte..self.cursor);
            self.cursor = byte;
        }
    }

    pub fn delete(&mut self) {
        self.fork_recall_on_edit();
        self.normalize_cursor();
        if let Some(grapheme) = self.text[self.cursor..].graphemes(true).next() {
            self.text.drain(self.cursor..self.cursor + grapheme.len());
        }
    }

    pub fn move_left(&mut self) {
        self.normalize_cursor();
        if let Some((byte, _)) = self.text[..self.cursor].grapheme_indices(true).next_back() {
            self.cursor = byte;
        }
    }

    pub fn move_right(&mut self) {
        self.normalize_cursor();
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
        self.history_index = None;
        self.history_draft.clear();
        self.history_draft_cursor = 0;
        if self.history.last() != Some(&submitted) {
            self.history.push(submitted.clone());
        }
        Some(submitted)
    }

    fn move_to_adjacent_row(&mut self, rows: &[EditorRow], from: usize, to: usize) {
        let source = &rows[from];
        let column = display_width(&self.text[source.start_byte..self.cursor.min(source.end_byte)]);
        let target = &rows[to];
        self.cursor =
            byte_at_display_column(&self.text, target.start_byte, target.end_byte, column);
    }

    fn recall_previous(&mut self) {
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
}
