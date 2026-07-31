use std::fmt;
use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

use crate::sanitize_text;
use crate::text::{byte_at_display_column, display_width, wrap_ranges};

const COMPACT_PASTE_LINE_THRESHOLD: usize = 4;
const COMPACT_PASTE_BYTE_THRESHOLD: usize = 400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorRow {
    pub start_byte: usize,
    pub end_byte: usize,
}

#[derive(Clone, PartialEq, Eq)]
pub struct PasteUnit {
    pub placeholder: String,
    pub content: String,
}

impl fmt::Debug for PasteUnit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PasteUnit")
            .field("placeholder", &self.placeholder)
            .field(
                "content",
                &format_args!("<{} bytes redacted>", self.content.len()),
            )
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ImageAttachment {
    pub placeholder: String,
    pub media_type: String,
    pub data: String,
}

impl fmt::Debug for ImageAttachment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ImageAttachment")
            .field("placeholder", &self.placeholder)
            .field("media_type", &self.media_type)
            .field(
                "data",
                &format_args!("<{} bytes redacted>", self.data.len()),
            )
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SubmittedInput {
    pub shown: String,
    pub display: String,
    pub prompt: String,
    pub pastes: Vec<PasteUnit>,
    pub images: Vec<ImageAttachment>,
}

impl fmt::Debug for SubmittedInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SubmittedInput")
            .field(
                "shown",
                &format_args!("<{} bytes redacted>", self.shown.len()),
            )
            .field(
                "display",
                &format_args!("<{} bytes redacted>", self.display.len()),
            )
            .field(
                "prompt",
                &format_args!("<{} bytes redacted>", self.prompt.len()),
            )
            .field("pastes", &self.pastes)
            .field("images", &self.images)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AtomicPayload {
    Paste(PasteUnit),
    Image(ImageAttachment),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveUnit {
    start: usize,
    payload: AtomicPayload,
}

impl ActiveUnit {
    fn placeholder(&self) -> &str {
        match &self.payload {
            AtomicPayload::Paste(paste) => &paste.placeholder,
            AtomicPayload::Image(image) => &image.placeholder,
        }
    }

    fn end(&self) -> usize {
        self.start.saturating_add(self.placeholder().len())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditorSnapshot {
    text: String,
    cursor: usize,
    units: Vec<ActiveUnit>,
}

impl EditorSnapshot {
    pub(crate) fn text(&self) -> &str {
        &self.text
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EditorToken {
    pub range: Range<usize>,
    pub query: String,
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
    history_draft_units: Vec<ActiveUnit>,
    units: Vec<ActiveUnit>,
    paste_sequence: usize,
    image_sequence: usize,
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
    pub(crate) fn snapshot(&self) -> EditorSnapshot {
        EditorSnapshot {
            text: self.text.clone(),
            cursor: self.cursor,
            units: self.units.clone(),
        }
    }

    pub(crate) fn restore_snapshot(&mut self, snapshot: EditorSnapshot) {
        self.text = snapshot.text;
        self.cursor = snapshot.cursor.min(self.text.len());
        self.units = snapshot.units;
        self.normalize_cursor();
        self.preferred_column = None;
        self.history_index = None;
        self.history_draft.clear();
        self.history_draft_cursor = 0;
        self.history_draft_units.clear();
    }

    #[must_use]
    pub(crate) fn slash_token(&self) -> Option<EditorToken> {
        if !self.text.starts_with('/') || self.cursor == 0 {
            return None;
        }
        let before = &self.text[..self.cursor];
        if before.contains(char::is_whitespace) {
            return None;
        }
        let end = self.text[self.cursor..]
            .find(char::is_whitespace)
            .map_or(self.text.len(), |offset| self.cursor + offset);
        Some(EditorToken {
            range: 0..end,
            query: before[1..].to_string(),
        })
    }

    #[must_use]
    pub(crate) fn mention_token(&self) -> Option<EditorToken> {
        let before = &self.text[..self.cursor];
        let start = before.rfind('@')?;
        if start > 0
            && !self.text[..start]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace)
        {
            return None;
        }
        let query = &before[start + 1..];
        if query.contains(char::is_whitespace) {
            return None;
        }
        let end = self.text[self.cursor..]
            .find(char::is_whitespace)
            .map_or(self.text.len(), |offset| self.cursor + offset);
        Some(EditorToken {
            range: start..end,
            query: query.to_string(),
        })
    }

    pub(crate) fn replace_range(&mut self, range: Range<usize>, replacement: &str) {
        self.fork_recall_on_edit();
        self.normalize_cursor();
        self.preferred_column = None;
        let replacement = sanitize_text(replacement);
        let start = self.delete_range_atomic(range.start, range.end);
        self.shift_units_at_or_after(start, replacement.len());
        self.text.insert_str(start, &replacement);
        self.cursor = start.saturating_add(replacement.len());
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
        self.units.clear();
        self.preferred_column = None;
        self.history_index = None;
        self.history_draft.clear();
        self.history_draft_cursor = 0;
        self.history_draft_units.clear();
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
            self.snap_cursor_to_nearest_unit_boundary();
            self.preferred_column = None;
        }
    }

    pub fn insert(&mut self, text: &str) {
        self.fork_recall_on_edit();
        self.normalize_cursor();
        self.preferred_column = None;
        let text = sanitize_text(text);
        self.snap_cursor_out_of_unit(true);
        self.shift_units_at_or_after(self.cursor, text.len());
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }

    pub fn insert_paste(&mut self, text: impl Into<String>) {
        let text = sanitize_text(&text.into());
        let lines = text.split('\n').count().max(1);
        if lines < COMPACT_PASTE_LINE_THRESHOLD && text.len() <= COMPACT_PASTE_BYTE_THRESHOLD {
            self.insert(&text);
            return;
        }

        self.fork_recall_on_edit();
        self.normalize_cursor();
        self.preferred_column = None;
        self.snap_cursor_out_of_unit(true);
        self.paste_sequence = self.paste_sequence.saturating_add(1);
        let placeholder = format!("[Paste #{} - {lines} lines]", self.paste_sequence);
        let start = self.cursor;
        self.shift_units_at_or_after(start, placeholder.len());
        self.text.insert_str(start, &placeholder);
        self.cursor = start.saturating_add(placeholder.len());
        self.units.push(ActiveUnit {
            start,
            payload: AtomicPayload::Paste(PasteUnit {
                placeholder,
                content: text,
            }),
        });
        self.units.sort_by_key(|unit| unit.start);
    }

    pub(crate) fn insert_image(
        &mut self,
        media_type: impl Into<String>,
        data: impl Into<String>,
        byte_len: usize,
    ) -> String {
        self.fork_recall_on_edit();
        self.normalize_cursor();
        self.preferred_column = None;
        self.snap_cursor_out_of_unit(true);
        self.image_sequence = self.image_sequence.saturating_add(1);
        let media_type = sanitize_text(&media_type.into());
        let label = media_type
            .rsplit('/')
            .next()
            .unwrap_or("image")
            .to_uppercase();
        let placeholder = format!(
            "[image #{} · {label} {}]",
            self.image_sequence,
            human_byte_size(byte_len)
        );
        let start = self.cursor;
        self.shift_units_at_or_after(start, placeholder.len());
        self.text.insert_str(start, &placeholder);
        self.cursor = start.saturating_add(placeholder.len());
        self.units.push(ActiveUnit {
            start,
            payload: AtomicPayload::Image(ImageAttachment {
                placeholder: placeholder.clone(),
                media_type,
                data: data.into(),
            }),
        });
        self.units.sort_by_key(|unit| unit.start);
        placeholder
    }

    pub fn backspace(&mut self) {
        self.fork_recall_on_edit();
        self.normalize_cursor();
        self.preferred_column = None;
        if let Some((start, end)) = self
            .units
            .iter()
            .find(|paste| self.cursor > paste.start && self.cursor <= paste.end())
            .map(|paste| (paste.start, paste.end()))
        {
            self.delete_range_atomic(start, end);
            self.cursor = start;
            return;
        }
        if let Some((byte, _)) = self.text[..self.cursor].grapheme_indices(true).next_back() {
            self.delete_range_atomic(byte, self.cursor);
            self.cursor = byte;
        }
    }

    pub fn delete(&mut self) {
        self.fork_recall_on_edit();
        self.normalize_cursor();
        self.preferred_column = None;
        if let Some((start, end)) = self
            .units
            .iter()
            .find(|paste| self.cursor >= paste.start && self.cursor < paste.end())
            .map(|paste| (paste.start, paste.end()))
        {
            self.delete_range_atomic(start, end);
            self.cursor = start;
            return;
        }
        if let Some(grapheme) = self.text[self.cursor..].graphemes(true).next() {
            self.delete_range_atomic(self.cursor, self.cursor + grapheme.len());
        }
    }

    pub fn move_left(&mut self) {
        self.normalize_cursor();
        self.preferred_column = None;
        if let Some(start) = self
            .units
            .iter()
            .find(|paste| self.cursor > paste.start && self.cursor <= paste.end())
            .map(|paste| paste.start)
        {
            self.cursor = start;
            return;
        }
        if let Some((byte, _)) = self.text[..self.cursor].grapheme_indices(true).next_back() {
            self.cursor = byte;
        }
    }

    pub fn move_right(&mut self) {
        self.normalize_cursor();
        self.preferred_column = None;
        if let Some(end) = self
            .units
            .iter()
            .find(|paste| self.cursor >= paste.start && self.cursor < paste.end())
            .map(ActiveUnit::end)
        {
            self.cursor = end;
            return;
        }
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

    pub fn submit(&mut self) -> Option<SubmittedInput> {
        if self.text.trim().is_empty() {
            return None;
        }
        let display = self.projected_text(false);
        let prompt = self.projected_text(true).trim().to_string();
        let shown = std::mem::take(&mut self.text);
        let mut pastes = Vec::new();
        let mut images = Vec::new();
        for unit in std::mem::take(&mut self.units) {
            match unit.payload {
                AtomicPayload::Paste(paste) => pastes.push(paste),
                AtomicPayload::Image(image) => images.push(image),
            }
        }
        self.cursor = 0;
        self.preferred_column = None;
        self.history_index = None;
        self.history_draft.clear();
        self.history_draft_cursor = 0;
        self.history_draft_units.clear();
        if self.history.last() != Some(&display) {
            self.history.push(display.clone());
        }
        Some(SubmittedInput {
            shown,
            display,
            prompt,
            pastes,
            images,
        })
    }

    pub fn move_visual_start(&mut self, width: u16) {
        self.normalize_cursor();
        let rows = self.visual_rows(width);
        let row = &rows[cursor_row(&rows, self.cursor)];
        self.cursor = row.start_byte;
        self.snap_cursor_out_of_unit(false);
        self.preferred_column = None;
    }

    pub fn move_visual_end(&mut self, width: u16) {
        self.normalize_cursor();
        let rows = self.visual_rows(width);
        let row = &rows[cursor_row(&rows, self.cursor)];
        self.cursor = row.end_byte;
        self.snap_cursor_out_of_unit(true);
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
        self.snap_cursor_out_of_unit(false);
        self.preferred_column = None;
    }

    pub fn move_word_right(&mut self) {
        self.normalize_cursor();
        self.cursor = next_word_end(&self.text, self.cursor);
        self.snap_cursor_out_of_unit(true);
        self.preferred_column = None;
    }

    pub fn delete_word_left(&mut self) {
        self.fork_recall_on_edit();
        self.normalize_cursor();
        self.preferred_column = None;
        let start = previous_word_start(&self.text, self.cursor);
        self.cursor = self.delete_range_atomic(start, self.cursor);
    }

    pub fn delete_to_line_start(&mut self) {
        self.fork_recall_on_edit();
        self.normalize_cursor();
        self.preferred_column = None;
        let start = self.line_start();
        self.cursor = self.delete_range_atomic(start, self.cursor);
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
        self.cursor = self.delete_range_atomic(self.cursor, delete_end);
    }

    fn move_to_adjacent_row(&mut self, rows: &[EditorRow], from: usize, to: usize) {
        let source = &rows[from];
        let column = *self.preferred_column.get_or_insert_with(|| {
            display_width(&self.text[source.start_byte..self.cursor.min(source.end_byte)])
        });
        let target = &rows[to];
        self.cursor =
            byte_at_display_column(&self.text, target.start_byte, target.end_byte, column);
        self.snap_cursor_to_nearest_unit_boundary();
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
                self.history_draft_units = std::mem::take(&mut self.units);
                self.history.len() - 1
            }
        };
        self.text = self.history[index].clone();
        self.units.clear();
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
            self.units.clear();
            self.cursor = self.text.len();
            self.history_index = Some(index + 1);
        } else {
            self.text = std::mem::take(&mut self.history_draft);
            self.cursor = self.history_draft_cursor.min(self.text.len());
            self.units = std::mem::take(&mut self.history_draft_units);
            self.history_draft_cursor = 0;
            self.history_index = None;
        }
    }

    fn fork_recall_on_edit(&mut self) {
        if self.history_index.take().is_some() {
            self.history_draft.clear();
            self.history_draft_cursor = 0;
            self.history_draft_units.clear();
        }
    }

    fn projected_text(&self, remove_images: bool) -> String {
        let mut projected = String::with_capacity(self.text.len());
        let mut copied = 0;
        for unit in &self.units {
            let end = unit.end();
            if unit.start < copied
                || end > self.text.len()
                || self.text.get(unit.start..end) != Some(unit.placeholder())
            {
                continue;
            }
            projected.push_str(&self.text[copied..unit.start]);
            match &unit.payload {
                AtomicPayload::Paste(paste) => projected.push_str(&paste.content),
                AtomicPayload::Image(image) if !remove_images => {
                    projected.push_str(&image.placeholder);
                }
                AtomicPayload::Image(_) => {}
            }
            copied = end;
        }
        projected.push_str(&self.text[copied..]);
        projected
    }

    fn snap_cursor_out_of_unit(&mut self, forward: bool) {
        if let Some((start, end)) = self
            .units
            .iter()
            .find(|unit| self.cursor > unit.start && self.cursor < unit.end())
            .map(|unit| (unit.start, unit.end()))
        {
            self.cursor = if forward { end } else { start };
        }
    }

    fn snap_cursor_to_nearest_unit_boundary(&mut self) {
        if let Some((start, end)) = self
            .units
            .iter()
            .find(|unit| self.cursor > unit.start && self.cursor < unit.end())
            .map(|unit| (unit.start, unit.end()))
        {
            self.cursor = if self.cursor - start <= end - self.cursor {
                start
            } else {
                end
            };
        }
    }

    fn shift_units_at_or_after(&mut self, at: usize, amount: usize) {
        if amount == 0 {
            return;
        }
        for unit in &mut self.units {
            if unit.start >= at {
                unit.start = unit.start.saturating_add(amount);
            }
        }
    }

    fn delete_range_atomic(&mut self, start: usize, end: usize) -> usize {
        let mut start = start.min(self.text.len());
        let mut end = end.min(self.text.len());
        if start >= end {
            return start;
        }

        loop {
            let mut expanded_start = start;
            let mut expanded_end = end;
            for unit in &self.units {
                if unit.start < end && unit.end() > start {
                    expanded_start = expanded_start.min(unit.start);
                    expanded_end = expanded_end.max(unit.end());
                }
            }
            if (expanded_start, expanded_end) == (start, end) {
                break;
            }
            start = expanded_start;
            end = expanded_end;
        }

        self.text.drain(start..end);
        let removed = end - start;
        self.units.retain_mut(|unit| {
            if unit.start >= start && unit.end() <= end {
                false
            } else {
                if unit.start >= end {
                    unit.start -= removed;
                }
                true
            }
        });
        start
    }

    fn normalize_cursor(&mut self) {
        self.cursor = self.cursor.min(self.text.len());
        while !self.text.is_char_boundary(self.cursor) {
            self.cursor = self.cursor.saturating_sub(1);
        }
    }
}

fn human_byte_size(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = 1024 * 1024;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{} KB", bytes / KB)
    } else {
        format!("{bytes} B")
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

    fn twelve_line_paste() -> String {
        (1..=12)
            .map(|line| format!("line {line} 界"))
            .collect::<Vec<_>>()
            .join("\n")
    }

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

    #[test]
    fn multiline_paste_is_one_compact_atomic_unit() {
        let mut editor = Editor::default();
        editor.insert_paste(twelve_line_paste());
        assert_eq!(editor.text(), "[Paste #1 - 12 lines]");

        editor.move_text_start();
        editor.move_right();
        assert_eq!(editor.cursor(), editor.text().len());
        editor.move_left();
        assert_eq!(editor.cursor(), 0);

        editor.set_cursor_from_visual(0, 19, 80);
        assert_eq!(editor.cursor(), editor.text().len());
        editor.backspace();
        assert_eq!(editor.text(), "");
        assert!(editor.units.is_empty());
    }

    #[test]
    fn compact_paste_submission_keeps_shown_text_and_expands_exact_prompt() {
        let payload = twelve_line_paste();
        let mut editor = Editor::default();
        editor.insert("before ");
        editor.insert_paste(payload.clone());
        editor.insert(" after");

        let submitted = editor.submit().expect("submitted paste");
        assert_eq!(submitted.shown, "before [Paste #1 - 12 lines] after");
        assert_eq!(submitted.prompt, format!("before {payload} after"));
        assert_eq!(
            submitted.pastes,
            vec![PasteUnit {
                placeholder: "[Paste #1 - 12 lines]".to_string(),
                content: payload,
            }]
        );
    }

    #[test]
    fn image_units_are_atomic_and_have_distinct_display_and_model_projections() {
        let mut editor = Editor::default();
        editor.insert("before ");
        let placeholder = editor.insert_image("image/png", "SECRET_BASE64_IMAGE", 2048);
        editor.insert(" after");
        assert_eq!(placeholder, "[image #1 · PNG 2 KB]");
        assert_eq!(editor.text(), "before [image #1 · PNG 2 KB] after");

        let submitted = editor.submit().expect("submitted image");
        assert_eq!(submitted.display, "before [image #1 · PNG 2 KB] after");
        assert_eq!(submitted.prompt, "before  after");
        assert!(submitted.pastes.is_empty());
        assert_eq!(submitted.images.len(), 1);
        assert_eq!(submitted.images[0].data, "SECRET_BASE64_IMAGE");
        let debug = format!("{submitted:?}");
        assert!(!debug.contains("SECRET_BASE64_IMAGE"));
        assert!(debug.contains("19 bytes redacted"));
    }

    #[test]
    fn editing_any_part_of_an_image_placeholder_removes_the_whole_unit() {
        let mut editor = Editor::default();
        editor.insert("left");
        let placeholder = editor.insert_image("image/png", "image-data", 10);
        editor.insert("right");
        editor.replace_range(5..6, "X");
        assert_eq!(editor.text(), "leftXright");
        assert!(editor.units.is_empty());

        let submitted = editor.submit().expect("text remains");
        assert!(submitted.images.is_empty());
        assert!(!submitted.display.contains(&placeholder));
    }

    #[test]
    fn history_round_trip_restores_an_unsubmitted_image_without_reconstructing_recall() {
        let mut editor = Editor::new(vec!["remembered".to_string()]);
        let placeholder = editor.insert_image("image/png", "draft-image", 512);
        editor.up_or_history(80);
        assert_eq!(editor.text(), "remembered");
        editor.down_or_history(80);
        assert_eq!(editor.text(), placeholder);

        let submitted = editor.submit().expect("restored image draft");
        assert_eq!(submitted.images.len(), 1);
        editor.up_or_history(80);
        assert_eq!(editor.text(), placeholder);
        assert!(
            editor.units.is_empty(),
            "recall must leave an inert placeholder"
        );
        let recalled = editor.submit().expect("inert recalled placeholder");
        assert!(recalled.images.is_empty());
        assert_eq!(recalled.prompt, placeholder);
    }

    #[test]
    fn paste_and_image_debug_views_redact_payloads() {
        let mut editor = Editor::default();
        editor.insert_paste(format!("{}\nline 2\nline 3\nline 4", "PASTE_SECRET"));
        editor.insert_image("image/png", "IMAGE_SECRET", 12);
        let debug = format!("{editor:?}");
        assert!(!debug.contains("PASTE_SECRET"));
        assert!(!debug.contains("IMAGE_SECRET"));
        assert!(debug.contains("bytes redacted"));
        let submitted = editor.submit().expect("secret-bearing draft");
        let debug = format!("{submitted:?}");
        assert!(!debug.contains("PASTE_SECRET"));
        assert!(!debug.contains("IMAGE_SECRET"));
    }

    #[test]
    fn submitted_paste_recalls_as_raw_multiline_text() {
        let payload = twelve_line_paste();
        let mut editor = Editor::default();
        editor.insert_paste(payload.clone());
        let _ = editor.submit();

        editor.up_or_history(80);
        assert_eq!(editor.text(), payload);
        assert!(editor.units.is_empty());
    }

    #[test]
    fn history_round_trip_restores_an_unsubmitted_compact_paste_draft() {
        let payload = twelve_line_paste();
        let mut editor = Editor::new(vec!["remembered".to_string()]);
        editor.insert_paste(payload.clone());
        editor.up_or_history(80);
        assert_eq!(editor.text(), "remembered");

        editor.down_or_history(80);
        assert_eq!(editor.text(), "[Paste #1 - 12 lines]");
        let submitted = editor.submit().expect("restored draft");
        assert_eq!(submitted.prompt, payload);
        assert_eq!(submitted.pastes.len(), 1);
    }

    #[test]
    fn a_kill_intersecting_a_paste_removes_the_whole_unit() {
        let mut editor = Editor::default();
        editor.insert("before ");
        editor.insert_paste(twelve_line_paste());
        editor.insert(" after");
        editor.move_word_left();
        editor.delete_to_line_start();

        assert_eq!(editor.text(), "after");
        assert!(editor.units.is_empty());
    }

    #[test]
    fn short_pastes_remain_inline_text() {
        let mut editor = Editor::default();
        editor.insert_paste("one\ntwo\nthree");
        assert_eq!(editor.text(), "one\ntwo\nthree");
        assert!(editor.units.is_empty());
    }

    #[test]
    fn slash_tokens_are_only_the_leading_unspaced_command_word() {
        let mut editor = Editor::default();
        editor.insert("/knw suffix");
        editor.move_text_start();
        editor.move_right();
        editor.move_right();
        editor.move_right();
        editor.move_right();
        assert_eq!(
            editor.slash_token(),
            Some(EditorToken {
                range: 0..4,
                query: "knw".to_string(),
            })
        );

        editor.move_text_end();
        assert!(editor.slash_token().is_none());
        editor.replace_draft("prefix /knw");
        assert!(editor.slash_token().is_none());
    }

    #[test]
    fn token_replacement_expands_across_any_intersected_paste_unit() {
        let mut editor = Editor::default();
        editor.insert("x");
        editor.insert_paste(twelve_line_paste());
        let inside_paste = 2;
        editor.insert("y");

        editor.replace_range(0..inside_paste, "z");
        assert_eq!(editor.text(), "zy");
        assert!(editor.units.is_empty());
        assert_eq!(editor.cursor(), 1);
    }

    #[test]
    fn mention_tokens_require_a_word_boundary_and_cover_the_whole_path_token() {
        let mut editor = Editor::default();
        editor.insert("ask @sam tail");
        editor.move_text_start();
        for _ in 0..8 {
            editor.move_right();
        }
        assert_eq!(
            editor.mention_token(),
            Some(EditorToken {
                range: 4..8,
                query: "sam".to_string(),
            })
        );

        editor.replace_draft("user@host");
        assert!(editor.mention_token().is_none());
    }
}
