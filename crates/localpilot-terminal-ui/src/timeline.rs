use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::sanitize_text;
use crate::text::wrap_ranges;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemId(u64);

impl ItemId {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    User,
    Assistant,
    Reasoning,
    Tool,
    Notice,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineItem {
    pub id: ItemId,
    pub kind: ItemKind,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualRow {
    pub item_id: ItemId,
    pub kind: ItemKind,
    pub start_byte: usize,
    pub end_byte: usize,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ContentPoint {
    pub item_id: ItemId,
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
    pub fn contains_grapheme(self, item_id: ItemId, start_byte: usize, end_byte: usize) -> bool {
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
pub enum ViewportAnchor {
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
    next_id: Option<u64>,
    pub viewport: ViewportAnchor,
    pub selection: Option<Selection>,
}

impl Default for Timeline {
    fn default() -> Self {
        Self::new()
    }
}

impl Timeline {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            items: Vec::new(),
            next_id: Some(1),
            viewport: ViewportAnchor::FollowBottom,
            selection: None,
        }
    }

    #[must_use]
    pub fn items(&self) -> &[TimelineItem] {
        &self.items
    }

    pub fn push(&mut self, kind: ItemKind, text: impl Into<String>) -> Option<ItemId> {
        let raw = self.next_id?;
        let id = ItemId(raw);
        self.next_id = raw.checked_add(1);
        self.items.push(TimelineItem {
            id,
            kind,
            text: sanitize_text(&text.into()),
        });
        Some(id)
    }

    pub fn append_text(&mut self, id: ItemId, text: &str) -> bool {
        let Some(item) = self.items.iter_mut().find(|item| item.id == id) else {
            return false;
        };
        item.text.push_str(&sanitize_text(text));
        true
    }

    #[must_use]
    pub fn rows(&self, width: u16) -> Vec<VisualRow> {
        self.items
            .iter()
            .flat_map(|item| {
                wrap_ranges(&item.text, width)
                    .into_iter()
                    .map(|range| VisualRow {
                        item_id: item.id,
                        kind: item.kind,
                        start_byte: range.start_byte,
                        end_byte: range.end_byte,
                        text: item.text[range.start_byte..range.end_byte].to_string(),
                    })
            })
            .collect()
    }

    #[must_use]
    pub fn view(&self, width: u16, height: u16) -> TimelineView {
        let all_rows = self.rows(width);
        let viewport_rows = usize::from(height.max(1));
        let max_start = all_rows.len().saturating_sub(viewport_rows);
        let start = match self.viewport {
            ViewportAnchor::FollowBottom => max_start,
            ViewportAnchor::Held(anchor) => resolve_anchor(&all_rows, anchor).min(max_start),
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
            ViewportAnchor::FollowBottom => max_start,
            ViewportAnchor::Held(anchor) => resolve_anchor(&rows, anchor).min(max_start),
        };
        let next = current.saturating_add_signed(delta).min(max_start);
        if next >= max_start {
            self.viewport = ViewportAnchor::FollowBottom;
        } else if let Some(row) = rows.get(next) {
            self.viewport = ViewportAnchor::Held(ContentPoint {
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
        let (start, end) = self.selection?.normalized();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_content_anchor_survives_stream_and_reflow() {
        let mut timeline = Timeline::new();
        for number in 0..20 {
            let _ = timeline.push(
                ItemKind::Assistant,
                format!("item {number} with wrapped text"),
            );
        }
        timeline.scroll_by(-5, 12, 5);
        let ViewportAnchor::Held(anchor) = timeline.viewport else {
            panic!("scrolling away from the bottom must hold a content anchor");
        };
        let _ = timeline.push(ItemKind::Assistant, "streamed after anchor");
        let narrow = timeline.view(9, 5);
        assert_eq!(
            narrow.rows.first().map(|row| row.item_id),
            Some(anchor.item_id)
        );
    }

    #[test]
    fn anchor_at_wrap_boundary_resolves_to_row_that_starts_there() {
        let mut timeline = Timeline::new();
        let id = timeline
            .push(ItemKind::Assistant, "abcdef")
            .expect("timeline id");
        timeline.viewport = ViewportAnchor::Held(ContentPoint {
            item_id: id,
            byte: 3,
        });
        let view = timeline.view(3, 1);
        assert_eq!(view.rows[0].start_byte, 3);
    }

    #[test]
    fn selection_extracts_exact_text_and_survives_append_and_resize() {
        let mut timeline = Timeline::new();
        let first = timeline
            .push(ItemKind::Assistant, "alpha 界 beta")
            .expect("first id");
        let second = timeline
            .push(ItemKind::Tool, "gamma 🧪 delta")
            .expect("second id");
        timeline.start_selection(ContentPoint {
            item_id: first,
            byte: "alpha ".len(),
        });
        timeline.extend_selection(ContentPoint {
            item_id: second,
            byte: "gamma 🧪".len(),
        });
        assert_eq!(
            timeline.selected_text().as_deref(),
            Some("界 beta\ngamma 🧪")
        );
        let _ = timeline.push(ItemKind::Notice, "later item");
        let _ = timeline.view(5, 3);
        assert_eq!(
            timeline.selected_text().as_deref(),
            Some("界 beta\ngamma 🧪")
        );
    }

    #[test]
    fn wide_grapheme_column_mapping_never_splits_utf8() {
        let mut timeline = Timeline::new();
        let id = timeline
            .push(ItemKind::Assistant, "a界🧪z")
            .expect("timeline id");
        let rows = timeline.rows(20);
        let point = Timeline::point_for_column(&rows[0], 2, false);
        let item = &timeline.items()[0];
        assert_eq!(point.item_id, id);
        assert!(item.text.is_char_boundary(point.byte));
        assert_eq!(&item.text[point.byte..], "界🧪z");
    }

    #[test]
    fn runtime_control_sequences_never_enter_timeline_state() {
        let mut timeline = Timeline::new();
        let id = timeline
            .push(ItemKind::Assistant, "safe\x1b[2J")
            .expect("timeline id");
        assert!(timeline.append_text(id, "still\x1b]0;owned\x07 safe"));
        assert_eq!(timeline.items()[0].text, "safestill safe");
    }
}
