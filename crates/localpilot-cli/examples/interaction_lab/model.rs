//! Pure state model for the terminal interaction lab.

use std::cmp::Ordering;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub const SEEDED_TIMELINE_ITEMS: usize = 500;
pub const MAX_STREAM_ITEMS: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    User,
    Assistant,
    Tool,
    Notice,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineItem {
    pub id: u64,
    pub kind: ItemKind,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualRow {
    pub item_id: u64,
    pub kind: ItemKind,
    pub start_byte: usize,
    pub end_byte: usize,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ContentPoint {
    pub item_id: u64,
    pub byte: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: ContentPoint,
    pub focus: ContentPoint,
}

impl Selection {
    #[must_use]
    pub fn normalized(self) -> (ContentPoint, ContentPoint) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }

    #[must_use]
    pub fn contains_grapheme(self, item_id: u64, start_byte: usize, end_byte: usize) -> bool {
        let (start, end) = self.normalized();
        let grapheme_start = ContentPoint {
            item_id,
            byte: start_byte,
        };
        let grapheme_end = ContentPoint {
            item_id,
            byte: end_byte,
        };
        grapheme_start < end && grapheme_end > start
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Viewport {
    FollowBottom,
    Held(ContentPoint),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineView {
    pub rows: Vec<VisualRow>,
    pub start: usize,
    pub total_rows: usize,
    pub viewport_rows: usize,
}

#[derive(Debug, Clone)]
pub struct Timeline {
    items: Vec<TimelineItem>,
    next_id: u64,
    pub viewport: Viewport,
    pub selection: Option<Selection>,
    pub stream_tick: usize,
    pub streaming: bool,
}

impl Timeline {
    #[must_use]
    pub fn seeded() -> Self {
        let mut timeline = Self::empty();
        for number in 1..=SEEDED_TIMELINE_ITEMS {
            let kind = if number % 41 == 0 {
                ItemKind::Tool
            } else if number % 29 == 0 {
                ItemKind::User
            } else if number % 17 == 0 {
                ItemKind::Notice
            } else {
                ItemKind::Assistant
            };
            timeline.push(
                kind,
                format!(
                    "{number:03} synthetic timeline item — styled text, wide 界, emoji 🧪, stable-id"
                ),
            );
        }
        timeline
    }

    #[must_use]
    pub fn empty() -> Self {
        Self {
            items: Vec::new(),
            next_id: 1,
            viewport: Viewport::FollowBottom,
            selection: None,
            stream_tick: 0,
            streaming: true,
        }
    }

    pub fn push(&mut self, kind: ItemKind, text: String) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(TimelineItem { id, kind, text });
        id
    }

    pub fn append_stream_tick(&mut self) {
        if !self.streaming || self.items.len() >= MAX_STREAM_ITEMS {
            return;
        }
        self.stream_tick += 1;
        self.push(
            ItemKind::Assistant,
            format!(
                "stream {:03} · incoming bytes preserve a held content anchor",
                self.stream_tick
            ),
        );
    }

    pub fn submit_user_text(&mut self, text: String) {
        self.push(ItemKind::User, text);
        self.viewport = Viewport::FollowBottom;
        self.selection = None;
    }

    #[must_use]
    pub fn item_count(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn rows(&self, width: u16) -> Vec<VisualRow> {
        let width = usize::from(width.max(1));
        self.items
            .iter()
            .flat_map(|item| wrap_item(item, width))
            .collect()
    }

    #[must_use]
    pub fn view(&self, width: u16, height: u16) -> TimelineView {
        let all_rows = self.rows(width);
        let viewport_rows = usize::from(height.max(1));
        let max_start = all_rows.len().saturating_sub(viewport_rows);
        let start = match self.viewport {
            Viewport::FollowBottom => max_start,
            Viewport::Held(anchor) => resolve_anchor(&all_rows, anchor).min(max_start),
        };
        let end = (start + viewport_rows).min(all_rows.len());
        TimelineView {
            rows: all_rows[start..end].to_vec(),
            start,
            total_rows: all_rows.len(),
            viewport_rows,
        }
    }

    pub fn scroll_by(&mut self, delta: isize, width: u16, height: u16) {
        let rows = self.rows(width);
        let viewport_rows = usize::from(height.max(1));
        let max_start = rows.len().saturating_sub(viewport_rows);
        let current = match self.viewport {
            Viewport::FollowBottom => max_start,
            Viewport::Held(anchor) => resolve_anchor(&rows, anchor).min(max_start),
        };
        let next = current.saturating_add_signed(delta).min(max_start);
        self.set_start(&rows, next, max_start);
    }

    pub fn jump_to_ratio(&mut self, ratio: f64, width: u16, height: u16) {
        let rows = self.rows(width);
        let viewport_rows = usize::from(height.max(1));
        let max_start = rows.len().saturating_sub(viewport_rows);
        let clamped = ratio.clamp(0.0, 1.0);
        let start = (clamped * max_start as f64).round() as usize;
        self.set_start(&rows, start, max_start);
    }

    fn set_start(&mut self, rows: &[VisualRow], start: usize, max_start: usize) {
        if start >= max_start {
            self.viewport = Viewport::FollowBottom;
        } else if let Some(row) = rows.get(start) {
            self.viewport = Viewport::Held(ContentPoint {
                item_id: row.item_id,
                byte: row.start_byte,
            });
        }
    }

    pub fn start_selection(&mut self, point: ContentPoint) {
        self.selection = Some(Selection {
            anchor: point,
            focus: point,
        });
    }

    pub fn extend_selection(&mut self, point: ContentPoint) {
        if let Some(selection) = self.selection.as_mut() {
            selection.focus = point;
        }
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    #[must_use]
    pub fn selected_text(&self) -> Option<String> {
        let selection = self.selection?;
        let (start, end) = selection.normalized();
        if start == end {
            return None;
        }
        let mut output = String::new();
        let mut selected_any = false;
        for item in &self.items {
            if item.id < start.item_id || item.id > end.item_id {
                continue;
            }
            let from = if item.id == start.item_id {
                start.byte.min(item.text.len())
            } else {
                0
            };
            let to = if item.id == end.item_id {
                end.byte.min(item.text.len())
            } else {
                item.text.len()
            };
            if from > to || !item.text.is_char_boundary(from) || !item.text.is_char_boundary(to) {
                continue;
            }
            if selected_any {
                output.push('\n');
            }
            output.push_str(&item.text[from..to]);
            selected_any = true;
        }
        selected_any.then_some(output)
    }

    #[must_use]
    pub fn point_for_column(row: &VisualRow, column: u16, trailing: bool) -> ContentPoint {
        let mut used = 0usize;
        let target = usize::from(column);
        for (relative, grapheme) in row.text.grapheme_indices(true) {
            let width = UnicodeWidthStr::width(grapheme).max(1);
            if target < used + width {
                let byte = if trailing {
                    row.start_byte + relative + grapheme.len()
                } else {
                    row.start_byte + relative
                };
                return ContentPoint {
                    item_id: row.item_id,
                    byte,
                };
            }
            used += width;
        }
        ContentPoint {
            item_id: row.item_id,
            byte: row.end_byte,
        }
    }
}

fn resolve_anchor(rows: &[VisualRow], anchor: ContentPoint) -> usize {
    rows.iter()
        .position(|row| row.item_id == anchor.item_id && row.start_byte == anchor.byte)
        .or_else(|| {
            rows.iter().position(|row| {
                row.item_id == anchor.item_id
                    && row.start_byte < anchor.byte
                    && anchor.byte <= row.end_byte
            })
        })
        .or_else(|| rows.iter().position(|row| row.item_id >= anchor.item_id))
        .unwrap_or_else(|| rows.len().saturating_sub(1))
}

fn wrap_item(item: &TimelineItem, width: usize) -> Vec<VisualRow> {
    wrap_text(item.id, item.kind, &item.text, width)
}

fn wrap_text(item_id: u64, kind: ItemKind, text: &str, width: usize) -> Vec<VisualRow> {
    if text.is_empty() {
        return vec![VisualRow {
            item_id,
            kind,
            start_byte: 0,
            end_byte: 0,
            text: String::new(),
        }];
    }

    let width = width.max(1);
    let mut rows = Vec::new();
    let mut row_start = 0usize;
    let mut used = 0usize;
    for (byte, grapheme) in text.grapheme_indices(true) {
        if grapheme == "\n" {
            push_visual_row(&mut rows, item_id, kind, text, row_start, byte);
            row_start = byte + grapheme.len();
            used = 0;
            continue;
        }
        let grapheme_width = UnicodeWidthStr::width(grapheme).max(1);
        if used > 0 && used + grapheme_width > width {
            push_visual_row(&mut rows, item_id, kind, text, row_start, byte);
            row_start = byte;
            used = 0;
        }
        used += grapheme_width;
    }
    push_visual_row(&mut rows, item_id, kind, text, row_start, text.len());
    rows
}

fn push_visual_row(
    rows: &mut Vec<VisualRow>,
    item_id: u64,
    kind: ItemKind,
    text: &str,
    start_byte: usize,
    end_byte: usize,
) {
    rows.push(VisualRow {
        item_id,
        kind,
        start_byte,
        end_byte,
        text: text[start_byte..end_byte].to_string(),
    });
}

#[derive(Debug, Clone)]
pub struct Editor {
    pub text: String,
    pub cursor: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
    history_draft_cursor: usize,
}

impl Editor {
    #[must_use]
    pub fn seeded() -> Self {
        let text = "draft line one\ndraft line two".to_string();
        Self {
            cursor: text.len(),
            text,
            history: vec![
                "older one-line prompt".to_string(),
                "newer first line\nnewer second line".to_string(),
            ],
            history_index: None,
            history_draft: String::new(),
            history_draft_cursor: 0,
        }
    }

    #[must_use]
    pub fn visual_rows(&self, width: u16) -> Vec<VisualRow> {
        wrap_text(0, ItemKind::User, &self.text, usize::from(width.max(1)))
    }

    #[must_use]
    pub fn cursor_row_and_column(&self, width: u16) -> (usize, u16) {
        let rows = self.visual_rows(width);
        let row_index = cursor_row(&rows, self.cursor);
        let row = &rows[row_index];
        let slice = &self.text[row.start_byte..self.cursor.min(row.end_byte)];
        (row_index, UnicodeWidthStr::width(slice) as u16)
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
        self.text.insert_str(self.cursor, text);
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

    fn move_to_adjacent_row(&mut self, rows: &[VisualRow], from: usize, to: usize) {
        let source = &rows[from];
        let column =
            UnicodeWidthStr::width(&self.text[source.start_byte..self.cursor.min(source.end_byte)]);
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

    #[cfg(test)]
    #[must_use]
    pub fn history_index(&self) -> Option<usize> {
        self.history_index
    }

    fn normalize_cursor(&mut self) {
        self.cursor = self.cursor.min(self.text.len());
        while !self.text.is_char_boundary(self.cursor) {
            self.cursor = self.cursor.saturating_sub(1);
        }
    }
}

fn cursor_row(rows: &[VisualRow], cursor: usize) -> usize {
    rows.iter()
        .rposition(|row| row.start_byte == cursor)
        .or_else(|| {
            rows.iter()
                .position(|row| row.start_byte < cursor && cursor <= row.end_byte)
        })
        .unwrap_or_else(|| rows.len().saturating_sub(1))
}

fn byte_at_display_column(text: &str, start: usize, end: usize, column: usize) -> usize {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Composer,
    ReverseSearch { selected: usize },
    Completion { selected: usize },
}

#[derive(Debug, Clone)]
pub struct LabState {
    pub timeline: Timeline,
    pub editor: Editor,
    pub focus: Focus,
    pub copy_on_select: bool,
    pub copy_status: String,
    pub quit: bool,
}

impl LabState {
    #[must_use]
    pub fn seeded() -> Self {
        Self {
            timeline: Timeline::seeded(),
            editor: Editor::seeded(),
            focus: Focus::Composer,
            copy_on_select: false,
            copy_status: "not copied".to_string(),
            quit: false,
        }
    }

    pub fn vertical(&mut self, direction: Ordering, editor_width: u16) {
        match &mut self.focus {
            Focus::Composer => match direction {
                Ordering::Less => self.editor.up_or_history(editor_width),
                Ordering::Greater => self.editor.down_or_history(editor_width),
                Ordering::Equal => {}
            },
            Focus::ReverseSearch { selected } | Focus::Completion { selected } => {
                if direction == Ordering::Less {
                    *selected = selected.saturating_sub(1);
                } else if direction == Ordering::Greater {
                    *selected = selected.saturating_add(1).min(2);
                }
            }
        }
    }

    pub fn submit(&mut self) {
        if let Some(text) = self.editor.submit() {
            self.timeline.submit_user_text(text);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarGeometry {
    pub thumb_top: u16,
    pub thumb_height: u16,
    pub max_start: usize,
}

#[must_use]
pub fn scrollbar_geometry(
    total_rows: usize,
    visible_rows: usize,
    start: usize,
    track_height: u16,
) -> ScrollbarGeometry {
    let track = usize::from(track_height.max(1));
    let max_start = total_rows.saturating_sub(visible_rows);
    if total_rows <= visible_rows || total_rows == 0 {
        return ScrollbarGeometry {
            thumb_top: 0,
            thumb_height: track_height.max(1),
            max_start,
        };
    }
    let thumb_height = ((visible_rows * track) / total_rows).max(1).min(track);
    let travel = track.saturating_sub(thumb_height);
    let thumb_top = if max_start == 0 {
        0
    } else {
        (start.min(max_start) * travel) / max_start
    };
    ScrollbarGeometry {
        thumb_top: thumb_top as u16,
        thumb_height: thumb_height as u16,
        max_start,
    }
}

#[must_use]
pub fn scrollbar_ratio(row: u16, track_height: u16) -> f64 {
    if track_height <= 1 {
        return 1.0;
    }
    f64::from(row.min(track_height - 1)) / f64::from(track_height - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn up_down_moves_then_recalls_and_restores_full_draft_and_caret() {
        let mut editor = Editor::seeded();
        let original = editor.text.clone();
        let original_cursor = editor.cursor;

        editor.up_or_history(80);
        assert_eq!(editor.history_index(), None);
        assert!(editor.cursor < original_cursor);
        let boundary_cursor = editor.cursor;

        editor.up_or_history(80);
        assert!(editor.history_index().is_some());
        assert_eq!(editor.text, "newer first line\nnewer second line");

        editor.down_or_history(80);
        assert_eq!(editor.text, original);
        assert_eq!(editor.cursor, boundary_cursor);
        assert_eq!(editor.history_index(), None);
    }

    #[test]
    fn editing_recalled_input_forks_it_from_draft_navigation() {
        let mut editor = Editor::seeded();
        editor.cursor = 0;
        editor.up_or_history(80);
        assert!(editor.history_index().is_some());
        editor.insert("edited ");
        let edited = editor.text.clone();
        editor.down_or_history(80);
        assert_eq!(editor.text, edited);
        assert_eq!(editor.history_index(), None);
    }

    #[test]
    fn search_and_completion_arrows_do_not_leak_into_history() {
        let mut state = LabState::seeded();
        let original = state.editor.text.clone();
        state.focus = Focus::ReverseSearch { selected: 1 };
        state.vertical(Ordering::Less, 80);
        assert_eq!(state.focus, Focus::ReverseSearch { selected: 0 });
        assert_eq!(state.editor.text, original);

        state.focus = Focus::Completion { selected: 1 };
        state.vertical(Ordering::Greater, 80);
        assert_eq!(state.focus, Focus::Completion { selected: 2 });
        assert_eq!(state.editor.text, original);
    }

    #[test]
    fn held_content_anchor_survives_stream_and_reflow() {
        let mut timeline = Timeline::seeded();
        timeline.scroll_by(-12, 52, 10);
        let Viewport::Held(anchor) = timeline.viewport else {
            panic!("scrolling away from bottom must hold a content anchor");
        };
        timeline.append_stream_tick();
        assert_eq!(timeline.viewport, Viewport::Held(anchor));
        let narrower = timeline.view(31, 10);
        let first = narrower.rows.first().expect("seeded rows");
        assert_eq!(first.item_id, anchor.item_id);
        assert!(first.start_byte <= anchor.byte && anchor.byte <= first.end_byte);
    }

    #[test]
    fn anchor_at_wrap_boundary_resolves_to_row_that_starts_there() {
        let mut timeline = Timeline::empty();
        let item_id = timeline.push(ItemKind::Assistant, "abcdefgh".to_string());
        timeline.viewport = Viewport::Held(ContentPoint { item_id, byte: 4 });
        let view = timeline.view(4, 1);
        assert_eq!(view.rows[0].text, "efgh");
    }

    #[test]
    fn cursor_at_wrap_boundary_is_on_following_visual_row() {
        let text = "abcdefghi".to_string();
        let editor = Editor {
            text,
            cursor: 4,
            history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            history_draft_cursor: 0,
        };
        assert_eq!(editor.cursor_row_and_column(4), (1, 0));
    }

    #[test]
    fn selection_extracts_exact_text_and_survives_stream_and_resize() {
        let mut timeline = Timeline::empty();
        let first = timeline.push(ItemKind::Assistant, "alpha 界 beta".to_string());
        let second = timeline.push(ItemKind::Tool, "gamma 🧪 delta".to_string());
        timeline.start_selection(ContentPoint {
            item_id: first,
            byte: "alpha ".len(),
        });
        timeline.extend_selection(ContentPoint {
            item_id: second,
            byte: "gamma 🧪".len(),
        });
        let before = timeline.selection;
        assert_eq!(
            timeline.selected_text().as_deref(),
            Some("界 beta\ngamma 🧪")
        );
        timeline.append_stream_tick();
        let _ = timeline.view(7, 4);
        let _ = timeline.view(40, 12);
        assert_eq!(timeline.selection, before);
        assert_eq!(
            timeline.selected_text().as_deref(),
            Some("界 beta\ngamma 🧪")
        );
    }

    #[test]
    fn submit_intentionally_reanchors_bottom() {
        let mut state = LabState::seeded();
        state.timeline.scroll_by(-20, 50, 8);
        assert!(matches!(state.timeline.viewport, Viewport::Held(_)));
        state.submit();
        assert_eq!(state.timeline.viewport, Viewport::FollowBottom);
        assert!(state.editor.text.is_empty());
    }

    #[test]
    fn wide_grapheme_column_mapping_never_splits_utf8() {
        let item = TimelineItem {
            id: 7,
            kind: ItemKind::Assistant,
            text: "a界b".to_string(),
        };
        let row = wrap_item(&item, 10).remove(0);
        let leading = Timeline::point_for_column(&row, 1, false);
        let trailing = Timeline::point_for_column(&row, 2, true);
        assert_eq!(leading.byte, 1);
        assert_eq!(trailing.byte, "a界".len());
        assert!(item.text.is_char_boundary(leading.byte));
        assert!(item.text.is_char_boundary(trailing.byte));
    }

    #[test]
    fn scrollbar_mapping_is_monotonic_and_bounded() {
        let geometry = scrollbar_geometry(1_000, 20, 400, 20);
        assert!(geometry.thumb_height >= 1);
        assert!(geometry.thumb_top + geometry.thumb_height <= 20);
        let mut previous = 0.0;
        for row in 0..20 {
            let ratio = scrollbar_ratio(row, 20);
            assert!((previous..=1.0).contains(&ratio));
            previous = ratio;
        }
        assert_eq!(scrollbar_ratio(19, 20), 1.0);
    }
}
