use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::sanitize_text;
use crate::text::{wrap_ranges, TextRow};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityState {
    Running,
    Success,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticRole {
    User,
    Assistant,
    Reasoning,
    Tool,
    Notice,
    Accent,
    Muted,
    Heading,
    Code,
    Link,
    Success,
    Error,
}

impl From<ItemKind> for SemanticRole {
    fn from(kind: ItemKind) -> Self {
        match kind {
            ItemKind::User => Self::User,
            ItemKind::Assistant => Self::Assistant,
            ItemKind::Reasoning => Self::Reasoning,
            ItemKind::Tool => Self::Tool,
            ItemKind::Notice => Self::Notice,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextStyle {
    pub role: SemanticRole,
    pub bold: bool,
    pub italic: bool,
    pub underlined: bool,
}

impl TextStyle {
    #[must_use]
    pub const fn new(role: SemanticRole) -> Self {
        Self {
            role,
            bold: false,
            italic: false,
            underlined: false,
        }
    }

    #[must_use]
    pub const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    #[must_use]
    pub const fn italic(mut self) -> Self {
        self.italic = true;
        self
    }

    #[must_use]
    pub const fn underlined(mut self) -> Self {
        self.underlined = true;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StyledRange {
    pub start_byte: usize,
    pub end_byte: usize,
    pub style: TextStyle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineItem {
    pub id: ItemId,
    pub kind: ItemKind,
    pub text: String,
    pub styles: Vec<StyledRange>,
    pub trailing: Option<String>,
    pub pending: bool,
    pub activity: Option<ActivityState>,
    pub expanded: bool,
}

impl TimelineItem {
    fn new(id: ItemId, kind: ItemKind, text: String) -> Self {
        let styles = full_range_style(&text, TextStyle::new(kind.into()));
        Self {
            id,
            kind,
            text,
            styles,
            trailing: None,
            pending: false,
            activity: None,
            expanded: !matches!(kind, ItemKind::Reasoning | ItemKind::Tool),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisualSpan {
    pub start_byte: usize,
    pub end_byte: usize,
    pub style: TextStyle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualRow {
    pub item_id: ItemId,
    pub kind: ItemKind,
    pub start_byte: usize,
    pub end_byte: usize,
    pub text: String,
    pub spans: Vec<VisualSpan>,
    pub part: VisualRowPart,
    pub content_column: u16,
    pub trailing: Option<String>,
    pub pending: bool,
    pub activity: Option<ActivityState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisualRowPart {
    FrameTop,
    Content { first: bool, last: bool },
    FrameBottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentPoint {
    pub item_id: ItemId,
    pub byte: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: ContentPoint,
    pub focus: ContentPoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewportAnchor {
    FollowBottom,
    Top,
    Held(ContentPoint),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineView {
    pub rows: Vec<VisualRow>,
    pub pinned: Option<PinnedPrompt>,
    pub new_content: bool,
    pub start: usize,
    pub total_rows: usize,
    pub viewport_rows: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedPrompt {
    pub item_id: ItemId,
    pub text: String,
    pub trailing: Option<String>,
    pub pending: bool,
    pub overflowing: bool,
}

impl PinnedPrompt {
    /// Top half-cell, content row, and bottom half-cell form the two-cell-high
    /// prompt surface used for both in-flow and pinned prompts.
    pub const ROWS: usize = 3;
}

#[derive(Debug, Clone)]
struct CachedWrap {
    width: u16,
    text_len: usize,
    ranges: Arc<[TextRow]>,
}

#[derive(Debug, Clone, Copy)]
struct ItemLayout {
    start: usize,
    end: usize,
}

#[derive(Debug, Clone)]
struct CachedLayout {
    width: u16,
    entries: Arc<[ItemLayout]>,
    total_rows: usize,
}

#[derive(Debug, Clone)]
pub struct Timeline {
    items: Vec<TimelineItem>,
    item_positions: HashMap<ItemId, usize>,
    wrap_cache: RefCell<HashMap<ItemId, CachedWrap>>,
    layout_cache: RefCell<Option<CachedLayout>>,
    next_id: Option<u64>,
    pub viewport: ViewportAnchor,
    pub selection: Option<Selection>,
    new_content: Cell<bool>,
}

impl Default for Timeline {
    fn default() -> Self {
        Self::new()
    }
}

impl Timeline {
    #[must_use]
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            item_positions: HashMap::new(),
            wrap_cache: RefCell::new(HashMap::new()),
            layout_cache: RefCell::new(None),
            next_id: Some(1),
            viewport: ViewportAnchor::FollowBottom,
            selection: None,
            new_content: Cell::new(false),
        }
    }

    #[must_use]
    pub fn items(&self) -> &[TimelineItem] {
        &self.items
    }

    #[must_use]
    pub fn item(&self, id: ItemId) -> Option<&TimelineItem> {
        self.item_positions
            .get(&id)
            .and_then(|index| self.items.get(*index))
    }

    pub fn push(&mut self, kind: ItemKind, text: impl Into<String>) -> Option<ItemId> {
        let raw = self.next_id?;
        let id = ItemId(raw);
        self.next_id = raw.checked_add(1);
        let text = sanitize_text(&text.into());
        self.item_positions.insert(id, self.items.len());
        self.items.push(TimelineItem::new(id, kind, text));
        if !matches!(self.viewport, ViewportAnchor::FollowBottom) {
            self.new_content.set(true);
        }
        self.invalidate_layout();
        Some(id)
    }

    /// Insert a new item immediately before an existing stable item. This is
    /// used when streamed output must remain attached to the active prompt
    /// while later user prompts are already visible as pending.
    pub fn insert_before(
        &mut self,
        before: ItemId,
        kind: ItemKind,
        text: impl Into<String>,
    ) -> Option<ItemId> {
        let index = self.item_positions.get(&before).copied()?;
        let raw = self.next_id?;
        let id = ItemId(raw);
        self.next_id = raw.checked_add(1);
        let text = sanitize_text(&text.into());
        self.items.insert(index, TimelineItem::new(id, kind, text));
        for position in index..self.items.len() {
            self.item_positions
                .insert(self.items[position].id, position);
        }
        if !matches!(self.viewport, ViewportAnchor::FollowBottom) {
            self.new_content.set(true);
        }
        self.invalidate_layout();
        Some(id)
    }

    pub fn append_text(&mut self, id: ItemId, text: &str) -> bool {
        let Some(index) = self.item_positions.get(&id).copied() else {
            return false;
        };
        let item = &mut self.items[index];
        let text = sanitize_text(text);
        if text.is_empty() {
            return true;
        }
        let old_len = item.text.len();
        item.text.push_str(&text);
        let base = TextStyle::new(item.kind.into());
        if let Some(last) = item.styles.last_mut().filter(|span| span.style == base) {
            last.end_byte = item.text.len();
        } else {
            item.styles.push(StyledRange {
                start_byte: old_len,
                end_byte: item.text.len(),
                style: base,
            });
        }
        self.wrap_cache.borrow_mut().remove(&id);
        if !matches!(self.viewport, ViewportAnchor::FollowBottom) {
            self.new_content.set(true);
        }
        self.invalidate_layout();
        true
    }

    pub fn set_styles(&mut self, id: ItemId, styles: Vec<StyledRange>) -> bool {
        let Some(index) = self.item_positions.get(&id).copied() else {
            return false;
        };
        let item = &mut self.items[index];
        item.styles = normalized_styles(item, styles);
        true
    }

    pub fn set_trailing(&mut self, id: ItemId, trailing: Option<String>) -> bool {
        let Some(index) = self.item_positions.get(&id).copied() else {
            return false;
        };
        self.items[index].trailing = trailing.map(|value| sanitize_text(&value));
        self.wrap_cache.borrow_mut().remove(&id);
        self.invalidate_layout();
        true
    }

    pub fn set_pending(&mut self, id: ItemId, pending: bool) -> bool {
        let Some(index) = self.item_positions.get(&id).copied() else {
            return false;
        };
        if self.items[index].kind != ItemKind::User || self.items[index].pending == pending {
            return self.items[index].kind == ItemKind::User;
        }
        self.items[index].pending = pending;
        self.wrap_cache.borrow_mut().remove(&id);
        self.invalidate_layout();
        true
    }

    pub fn set_activity(&mut self, id: ItemId, activity: Option<ActivityState>) -> bool {
        let Some(index) = self.item_positions.get(&id).copied() else {
            return false;
        };
        self.items[index].activity = activity;
        true
    }

    pub fn set_expanded(&mut self, id: ItemId, expanded: bool) -> bool {
        let Some(index) = self.item_positions.get(&id).copied() else {
            return false;
        };
        self.items[index].expanded = expanded;
        self.wrap_cache.borrow_mut().remove(&id);
        self.invalidate_layout();
        true
    }

    #[must_use]
    pub fn rows(&self, width: u16) -> Vec<VisualRow> {
        self.items
            .iter()
            .flat_map(|item| {
                let ranges = self.row_ranges(item, item_content_width(item, width));
                project_item_rows(item, &ranges, 0, item_visual_row_count(item, &ranges))
            })
            .collect()
    }

    #[must_use]
    pub fn view(&self, width: u16, height: u16) -> TimelineView {
        let outer_rows = usize::from(height.max(1));
        let total_rows = self.total_rows(width);
        let full_start = self
            .current_start(width)
            .min(total_rows.saturating_sub(outer_rows));
        let full_pin = (outer_rows > PinnedPrompt::ROWS)
            .then(|| self.pinned_prompt(width, full_start, outer_rows))
            .flatten();
        let (start, pinned, viewport_rows) = if let Some(full_pin) = full_pin {
            let pinned_rows = outer_rows.saturating_sub(PinnedPrompt::ROWS);
            let pinned_start = self
                .current_start(width)
                .min(total_rows.saturating_sub(pinned_rows));
            let pinned = self.pinned_prompt(width, pinned_start, outer_rows);
            if pinned
                .as_ref()
                .is_some_and(|value| value.item_id == full_pin.item_id)
            {
                (pinned_start, pinned, pinned_rows)
            } else {
                // Reserving the pinned surface crossed into the next prompt.
                // Prefer a full unpinned viewport at that exact boundary.
                (full_start, None, outer_rows)
            }
        } else {
            (full_start, None, outer_rows)
        };
        let end = start.saturating_add(viewport_rows).min(total_rows);
        if start >= total_rows.saturating_sub(viewport_rows) {
            self.new_content.set(false);
        }
        let rows = self.project_rows(width, start, end);
        TimelineView {
            rows,
            pinned,
            new_content: self.new_content.get(),
            start,
            total_rows,
            viewport_rows,
        }
    }

    pub fn scroll_by(&mut self, delta: isize, width: u16, height: u16) {
        let viewport_rows = usize::from(height.max(1));
        let total_rows = self.total_rows(width);
        let max_start = total_rows.saturating_sub(viewport_rows);
        let current = self.current_start(width).min(max_start);
        let next = current.saturating_add_signed(delta).min(max_start);
        self.set_viewport_start(next, width, max_start);
    }

    pub fn scroll_to_row(&mut self, start: usize, width: u16, height: u16) {
        let viewport_rows = usize::from(height.max(1));
        let max_start = self.total_rows(width).saturating_sub(viewport_rows);
        self.set_viewport_start(start.min(max_start), width, max_start);
    }

    fn set_viewport_start(&mut self, start: usize, width: u16, max_start: usize) {
        if start >= max_start {
            self.viewport = ViewportAnchor::FollowBottom;
            self.new_content.set(false);
        } else if start == 0 {
            self.viewport = ViewportAnchor::Top;
        } else if let Some(point) = self.point_at_row(width, start) {
            self.viewport = ViewportAnchor::Held(point);
        }
    }

    pub fn follow_bottom(&mut self) {
        self.viewport = ViewportAnchor::FollowBottom;
        self.new_content.set(false);
    }

    /// Hold the viewport at a stable content identity. The byte is validated
    /// against the original item text so resize and streaming reflow can safely
    /// resolve it to a visual row later.
    pub fn hold_at(&mut self, point: ContentPoint) -> bool {
        let Some(item) = self.item(point.item_id) else {
            return false;
        };
        if point.byte > item.text.len() || !item.text.is_char_boundary(point.byte) {
            return false;
        }
        self.viewport = ViewportAnchor::Held(point);
        true
    }

    #[must_use]
    pub fn has_new_content(&self) -> bool {
        self.new_content.get()
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
        let (start_index, start_byte, end_index, end_byte) = self.selection_bounds(selection)?;
        if start_index == end_index && start_byte == end_byte {
            return None;
        }
        let mut output = String::new();
        let mut selected_any = false;
        for (index, item) in self.items[start_index..=end_index].iter().enumerate() {
            let index = start_index + index;
            let visible_len = displayed_text(item).len();
            let from = if index == start_index {
                start_byte.min(visible_len)
            } else {
                0
            };
            let to = if index == end_index {
                end_byte.min(visible_len)
            } else {
                visible_len
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
    pub fn selection_contains_grapheme(
        &self,
        item_id: ItemId,
        start_byte: usize,
        end_byte: usize,
    ) -> bool {
        let Some(selection) = self.selection else {
            return false;
        };
        let Some((start_index, selection_start, end_index, selection_end)) =
            self.selection_bounds(selection)
        else {
            return false;
        };
        let Some(index) = self.item_positions.get(&item_id).copied() else {
            return false;
        };
        (index, start_byte) < (end_index, selection_end)
            && (index, end_byte) > (start_index, selection_start)
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

    fn row_ranges(&self, item: &TimelineItem, width: u16) -> Arc<[TextRow]> {
        let width = width.max(1);
        let display_text = displayed_text(item);
        if let Some(cached) = self.wrap_cache.borrow().get(&item.id) {
            if cached.width == width && cached.text_len == display_text.len() {
                return Arc::clone(&cached.ranges);
            }
        }
        let ranges: Arc<[TextRow]> = wrap_ranges(display_text, width).into();
        self.wrap_cache.borrow_mut().insert(
            item.id,
            CachedWrap {
                width,
                text_len: display_text.len(),
                ranges: Arc::clone(&ranges),
            },
        );
        ranges
    }

    fn total_rows(&self, width: u16) -> usize {
        self.layout_index(width).total_rows
    }

    fn current_start(&self, width: u16) -> usize {
        match self.viewport {
            ViewportAnchor::FollowBottom => self.total_rows(width),
            ViewportAnchor::Top => 0,
            ViewportAnchor::Held(anchor) => self.resolve_anchor(width, anchor),
        }
    }

    fn selection_bounds(&self, selection: Selection) -> Option<(usize, usize, usize, usize)> {
        let anchor_index = self
            .item_positions
            .get(&selection.anchor.item_id)
            .copied()?;
        let focus_index = self.item_positions.get(&selection.focus.item_id).copied()?;
        let anchor = (anchor_index, selection.anchor.byte);
        let focus = (focus_index, selection.focus.byte);
        if anchor <= focus {
            Some((anchor.0, anchor.1, focus.0, focus.1))
        } else {
            Some((focus.0, focus.1, anchor.0, anchor.1))
        }
    }

    fn resolve_anchor(&self, width: u16, anchor: ContentPoint) -> usize {
        let Some(index) = self.item_positions.get(&anchor.item_id).copied() else {
            return self.total_rows(width).saturating_sub(1);
        };
        let layout = self.layout_index(width);
        let Some(entry) = layout.entries.get(index) else {
            return layout.total_rows.saturating_sub(1);
        };
        let item = &self.items[index];
        let ranges = self.row_ranges(item, item_content_width(item, width));
        let within = ranges
            .iter()
            .position(|row| row.start_byte == anchor.byte)
            .or_else(|| {
                ranges
                    .iter()
                    .position(|row| row.start_byte < anchor.byte && anchor.byte <= row.end_byte)
            })
            .unwrap_or_else(|| ranges.len().saturating_sub(1));
        entry
            .start
            .saturating_add(frame_prefix_rows(item.kind))
            .saturating_add(within)
    }

    fn point_at_row(&self, width: u16, target: usize) -> Option<ContentPoint> {
        let layout = self.layout_index(width);
        let index = layout.entries.partition_point(|entry| entry.end <= target);
        let entry = layout.entries.get(index)?;
        let item = self.items.get(index)?;
        let local = target.saturating_sub(entry.start);
        let prefix = frame_prefix_rows(item.kind);
        if local < prefix {
            return Some(ContentPoint {
                item_id: item.id,
                byte: 0,
            });
        }
        let content_index = local.saturating_sub(prefix);
        let ranges = self.row_ranges(item, item_content_width(item, width));
        if let Some(row) = ranges.get(content_index) {
            return Some(ContentPoint {
                item_id: item.id,
                byte: row.start_byte,
            });
        }
        Some(ContentPoint {
            item_id: item.id,
            byte: item.text.len(),
        })
    }

    fn project_rows(&self, width: u16, start: usize, end: usize) -> Vec<VisualRow> {
        if start >= end {
            return Vec::new();
        }
        let mut output = Vec::with_capacity(end.saturating_sub(start));
        let layout = self.layout_index(width);
        let first_index = layout.entries.partition_point(|entry| entry.end <= start);
        for (index, entry) in layout.entries.iter().enumerate().skip(first_index) {
            let item = &self.items[index];
            let ranges = self.row_ranges(item, item_content_width(item, width));
            if entry.start >= end {
                break;
            }
            let item_rows = entry.end.saturating_sub(entry.start);
            let first = start.saturating_sub(entry.start);
            let last = end.saturating_sub(entry.start).min(item_rows);
            output.extend(project_item_rows(item, &ranges, first, last));
        }
        output
    }

    fn pinned_prompt(
        &self,
        width: u16,
        start: usize,
        viewport_rows: usize,
    ) -> Option<PinnedPrompt> {
        if start == 0 {
            return None;
        }
        let visible = self.point_at_row(width, start)?;
        let visible_index = self.item_positions.get(&visible.item_id).copied()?;
        let prompt_index = self.items[..=visible_index]
            .iter()
            .rposition(|item| item.kind == ItemKind::User)?;
        if prompt_index == visible_index {
            return None;
        }
        let prompt = &self.items[prompt_index];
        let group_end = self.items[prompt_index + 1..]
            .iter()
            .position(|item| item.kind == ItemKind::User)
            .map_or(self.items.len(), |offset| prompt_index + 1 + offset);
        if visible_index >= group_end {
            return None;
        }
        let response_rows = self.items[prompt_index + 1..group_end]
            .first()
            .map_or(0, |_| {
                let layout = self.layout_index(width);
                let start = layout.entries[prompt_index].end;
                let end = if group_end < layout.entries.len() {
                    layout.entries[group_end].start
                } else {
                    layout.total_rows
                };
                end.saturating_sub(start)
            });
        Some(PinnedPrompt {
            item_id: prompt.id,
            text: prompt.text.replace('\n', " "),
            trailing: prompt.trailing.clone(),
            pending: prompt.pending,
            overflowing: response_rows > viewport_rows,
        })
    }

    fn layout_index(&self, width: u16) -> CachedLayout {
        let width = width.max(1);
        if let Some(cached) = self
            .layout_cache
            .borrow()
            .as_ref()
            .filter(|cached| cached.width == width)
        {
            return cached.clone();
        }
        let mut start = 0usize;
        let entries: Arc<[ItemLayout]> = self
            .items
            .iter()
            .map(|item| {
                let ranges = self.row_ranges(item, item_content_width(item, width));
                let end = start.saturating_add(item_visual_row_count(item, &ranges));
                let entry = ItemLayout { start, end };
                start = end;
                entry
            })
            .collect::<Vec<_>>()
            .into();
        let cached = CachedLayout {
            width,
            entries,
            total_rows: start,
        };
        *self.layout_cache.borrow_mut() = Some(cached.clone());
        cached
    }

    fn invalidate_layout(&self) {
        *self.layout_cache.borrow_mut() = None;
    }
}

fn frame_prefix_rows(kind: ItemKind) -> usize {
    usize::from(kind == ItemKind::User)
}

fn displayed_text(item: &TimelineItem) -> &str {
    if item.expanded {
        return &item.text;
    }
    item.text
        .split_once('\n')
        .map_or(item.text.as_str(), |(summary, _)| summary)
}

fn item_content_width(item: &TimelineItem, width: u16) -> u16 {
    let decoration = match item.kind {
        ItemKind::User => 5u16.saturating_add(
            item.trailing
                .as_deref()
                .map(UnicodeWidthStr::width)
                .and_then(|value| u16::try_from(value).ok())
                .unwrap_or(0)
                .saturating_add(if item.pending { 10 } else { 0 })
                .saturating_add(1),
        ),
        ItemKind::Assistant | ItemKind::Reasoning | ItemKind::Tool | ItemKind::Notice => 2,
    };
    width.saturating_sub(decoration).max(1)
}

fn item_visual_row_count(item: &TimelineItem, ranges: &[TextRow]) -> usize {
    ranges
        .len()
        .saturating_add(if item.kind == ItemKind::User { 2 } else { 0 })
}

fn project_item_rows(
    item: &TimelineItem,
    ranges: &[TextRow],
    start: usize,
    end: usize,
) -> Vec<VisualRow> {
    let mut output = Vec::with_capacity(end.saturating_sub(start));
    let prefix = frame_prefix_rows(item.kind);
    for index in start..end {
        if item.kind == ItemKind::User && index == 0 {
            output.push(frame_row(item, VisualRowPart::FrameTop));
            continue;
        }
        let content_index = index.saturating_sub(prefix);
        if let Some(range) = ranges.get(content_index) {
            output.push(visual_row(
                item,
                *range,
                VisualRowPart::Content {
                    first: content_index == 0,
                    last: content_index + 1 == ranges.len(),
                },
            ));
        } else if item.kind == ItemKind::User {
            output.push(frame_row(item, VisualRowPart::FrameBottom));
        }
    }
    output
}

fn frame_row(item: &TimelineItem, part: VisualRowPart) -> VisualRow {
    let byte = if part == VisualRowPart::FrameTop {
        0
    } else {
        item.text.len()
    };
    VisualRow {
        item_id: item.id,
        kind: item.kind,
        start_byte: byte,
        end_byte: byte,
        text: String::new(),
        spans: Vec::new(),
        part,
        content_column: 0,
        trailing: None,
        pending: item.pending,
        activity: item.activity,
    }
}

fn full_range_style(text: &str, style: TextStyle) -> Vec<StyledRange> {
    (!text.is_empty())
        .then_some(StyledRange {
            start_byte: 0,
            end_byte: text.len(),
            style,
        })
        .into_iter()
        .collect()
}

fn normalized_styles(item: &TimelineItem, mut styles: Vec<StyledRange>) -> Vec<StyledRange> {
    let base = TextStyle::new(item.kind.into());
    styles.sort_by_key(|span| (span.start_byte, span.end_byte));
    let mut output = Vec::new();
    let mut cursor = 0usize;
    for span in styles {
        let start = span.start_byte.max(cursor).min(item.text.len());
        let end = span.end_byte.max(start).min(item.text.len());
        if start == end || !item.text.is_char_boundary(start) || !item.text.is_char_boundary(end) {
            continue;
        }
        if cursor < start {
            output.push(StyledRange {
                start_byte: cursor,
                end_byte: start,
                style: base,
            });
        }
        output.push(StyledRange {
            start_byte: start,
            end_byte: end,
            style: span.style,
        });
        cursor = end;
    }
    if cursor < item.text.len() {
        output.push(StyledRange {
            start_byte: cursor,
            end_byte: item.text.len(),
            style: base,
        });
    }
    output
}

fn visual_row(item: &TimelineItem, range: TextRow, part: VisualRowPart) -> VisualRow {
    let spans = item
        .styles
        .iter()
        .filter_map(|span| {
            let start = span.start_byte.max(range.start_byte);
            let end = span.end_byte.min(range.end_byte);
            (start < end).then_some(VisualSpan {
                start_byte: start,
                end_byte: end,
                style: span.style,
            })
        })
        .collect();
    VisualRow {
        item_id: item.id,
        kind: item.kind,
        start_byte: range.start_byte,
        end_byte: range.end_byte,
        text: item.text[range.start_byte..range.end_byte].to_string(),
        spans,
        part,
        content_column: match item.kind {
            ItemKind::User => 3,
            ItemKind::Assistant | ItemKind::Reasoning | ItemKind::Tool | ItemKind::Notice => 2,
        },
        trailing: matches!(part, VisualRowPart::Content { first: true, .. })
            .then(|| item.trailing.clone())
            .flatten(),
        pending: item.pending,
        activity: item.activity,
    }
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
    fn absolute_start_has_a_stable_top_anchor() {
        let mut timeline = Timeline::new();
        for number in 0..30 {
            let _ = timeline.push(ItemKind::Assistant, format!("item {number:02}"));
        }

        timeline.scroll_by(-10_000, 40, 8);
        assert_eq!(timeline.viewport, ViewportAnchor::Top);
        assert_eq!(timeline.view(40, 8).start, 0);

        let _ = timeline.push(ItemKind::Assistant, "new output while at top");
        assert_eq!(timeline.viewport, ViewportAnchor::Top);
        assert!(timeline.has_new_content());

        timeline.scroll_by(10_000, 40, 8);
        assert_eq!(timeline.viewport, ViewportAnchor::FollowBottom);
        assert!(!timeline.has_new_content());
    }

    #[test]
    fn absolute_scroll_target_uses_content_anchors_and_reaches_bottom() {
        let mut timeline = Timeline::new();
        for number in 0..40 {
            let _ = timeline.push(ItemKind::Assistant, format!("item {number:02}"));
        }

        timeline.scroll_to_row(12, 40, 8);
        let ViewportAnchor::Held(anchor) = timeline.viewport else {
            panic!("an interior absolute target must hold a content anchor");
        };
        assert_eq!(timeline.view(40, 8).start, 12);
        assert_eq!(
            timeline.view(40, 8).rows.first().map(|row| row.item_id),
            Some(anchor.item_id)
        );

        timeline.scroll_to_row(usize::MAX, 40, 8);
        assert_eq!(timeline.viewport, ViewportAnchor::FollowBottom);
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

    #[test]
    fn view_projects_only_visible_rows_but_matches_the_full_projection() {
        let mut timeline = Timeline::new();
        for number in 0..40 {
            let _ = timeline.push(ItemKind::Assistant, format!("row {number:02}"));
        }
        timeline.scroll_by(-7, 40, 8);
        let full = timeline.rows(40);
        let view = timeline.view(40, 8);
        assert_eq!(view.rows, full[view.start..view.start + view.rows.len()]);
        assert_eq!(view.total_rows, full.len());
    }

    #[test]
    fn ten_thousand_items_keep_a_bounded_visible_projection() {
        let mut timeline = Timeline::new();
        for number in 0..10_000 {
            let _ = timeline.push(ItemKind::Assistant, format!("item {number:05}"));
        }
        let view = timeline.view(40, 12);
        assert_eq!(view.total_rows, 10_000);
        assert_eq!(view.rows.len(), 12);
        assert_eq!(view.rows[0].text, "item 09988");
    }

    #[test]
    fn semantic_spans_survive_wrap_boundaries() {
        let mut timeline = Timeline::new();
        let id = timeline
            .push(ItemKind::Assistant, "abcdef")
            .expect("timeline id");
        assert!(timeline.set_styles(
            id,
            vec![StyledRange {
                start_byte: 1,
                end_byte: 5,
                style: TextStyle::new(SemanticRole::Link).underlined(),
            }]
        ));
        let rows = timeline.rows(5);
        assert_eq!(rows[0].spans.len(), 2);
        assert_eq!(rows[0].spans[1].end_byte, 3);
        assert_eq!(rows[1].spans[0].start_byte, 3);
        assert_eq!(rows[1].spans[0].end_byte, 5);
        assert_eq!(rows[1].spans[0].style.role, SemanticRole::Link);
    }

    #[test]
    fn user_prompts_project_frame_rows_and_reserve_trailing_metadata() {
        let mut timeline = Timeline::new();
        let id = timeline
            .push(ItemKind::User, "a prompt that wraps")
            .expect("timeline id");
        assert!(timeline.set_trailing(id, Some("12:34".to_string())));
        let rows = timeline.rows(20);
        assert_eq!(
            rows.first().map(|row| row.part),
            Some(VisualRowPart::FrameTop)
        );
        assert_eq!(
            rows.last().map(|row| row.part),
            Some(VisualRowPart::FrameBottom)
        );
        assert!(rows[1].trailing.is_some());
        assert!(rows[2].trailing.is_none());
        assert!(rows.iter().any(|row| row.text.starts_with("a prompt")));
    }

    #[test]
    fn pending_is_prompt_only_and_part_of_prompt_geometry() {
        let mut timeline = Timeline::new();
        let prompt = timeline.push(ItemKind::User, "queued").expect("prompt");
        let reply = timeline.push(ItemKind::Assistant, "reply").expect("reply");
        let ordinary = timeline.rows(20);

        assert!(timeline.set_pending(prompt, true));
        assert!(timeline.item(prompt).expect("prompt").pending);
        assert!(!timeline.set_pending(reply, true));
        assert!(!timeline.item(reply).expect("reply").pending);
        assert_ne!(timeline.rows(20), ordinary);
        assert!(timeline.set_pending(prompt, false));
        assert!(!timeline.item(prompt).expect("prompt").pending);
    }

    #[test]
    fn insertion_before_pending_prompt_preserves_every_stable_id() {
        let mut timeline = Timeline::new();
        let active = timeline.push(ItemKind::User, "active").expect("active");
        let pending = timeline.push(ItemKind::User, "pending").expect("pending");
        assert!(timeline.set_pending(pending, true));

        let response = timeline
            .insert_before(pending, ItemKind::Assistant, "response")
            .expect("response");
        assert_eq!(
            timeline
                .items()
                .iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![active, response, pending]
        );
        assert_eq!(timeline.item(active).expect("active").text, "active");
        assert_eq!(timeline.item(pending).expect("pending").text, "pending");
        assert!(timeline.append_text(response, " tail"));
        assert_eq!(
            timeline.item(response).expect("response").text,
            "response tail"
        );
    }

    #[test]
    fn selection_uses_visual_order_after_insertion_before_pending_prompt() {
        let mut timeline = Timeline::new();
        let active = timeline.push(ItemKind::User, "active").expect("active");
        let pending = timeline.push(ItemKind::User, "pending").expect("pending");
        let response = timeline
            .insert_before(pending, ItemKind::Assistant, "response")
            .expect("response");

        timeline.start_selection(ContentPoint {
            item_id: active,
            byte: 0,
        });
        timeline.extend_selection(ContentPoint {
            item_id: pending,
            byte: "pending".len(),
        });
        assert_eq!(
            timeline.selected_text().as_deref(),
            Some("active\nresponse\npending")
        );
        assert!(timeline.selection_contains_grapheme(response, 0, "response".len()));

        timeline.start_selection(ContentPoint {
            item_id: pending,
            byte: "pending".len(),
        });
        timeline.extend_selection(ContentPoint {
            item_id: active,
            byte: 0,
        });
        assert_eq!(
            timeline.selected_text().as_deref(),
            Some("active\nresponse\npending")
        );
    }

    #[test]
    fn overflowing_response_keeps_its_prompt_as_a_single_line_pin() {
        let mut timeline = Timeline::new();
        let prompt = timeline
            .push(ItemKind::User, "write many numbered lines")
            .expect("prompt id");
        assert!(timeline.set_trailing(prompt, Some("12:34".to_string())));
        for number in 0..40 {
            let _ = timeline.push(ItemKind::Assistant, format!("line {number:02}"));
        }
        let view = timeline.view(40, 8);
        let pin = view.pinned.expect("pinned prompt");
        assert_eq!(pin.item_id, prompt);
        assert!(pin.overflowing);
        assert_eq!(pin.text, "write many numbered lines");
        assert_eq!(view.rows.len(), 5);
        assert_eq!(
            view.rows.last().map(|row| row.text.as_str()),
            Some("line 39")
        );
    }

    #[test]
    fn activity_details_expand_in_place_without_a_second_timeline_item() {
        let mut timeline = Timeline::new();
        let tool = timeline
            .push(ItemKind::Tool, "read_file completed\nline one\nline two")
            .expect("tool id");
        assert!(timeline.set_activity(tool, Some(ActivityState::Success)));
        let collapsed = timeline.rows(40);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].text, "read_file completed");
        assert_eq!(collapsed[0].activity, Some(ActivityState::Success));
        timeline.start_selection(ContentPoint {
            item_id: tool,
            byte: 0,
        });
        timeline.extend_selection(ContentPoint {
            item_id: tool,
            byte: timeline.item(tool).expect("tool").text.len(),
        });
        assert_eq!(
            timeline.selected_text().as_deref(),
            Some("read_file completed")
        );

        assert!(timeline.set_expanded(tool, true));
        let expanded = timeline.rows(40);
        assert_eq!(expanded.len(), 3);
        assert_eq!(expanded[2].text, "line two");
        assert_eq!(timeline.items().len(), 1);
    }

    #[test]
    fn mixed_projection_matches_full_rows_with_frames_collapsed_items_and_pin() {
        let mut timeline = Timeline::new();
        let prompt = timeline
            .push(ItemKind::User, "first prompt with enough text to wrap")
            .expect("prompt id");
        assert!(timeline.set_trailing(prompt, Some("12:34".to_string())));
        for number in 0..24 {
            let _ = timeline.push(ItemKind::Assistant, format!("answer {number:02}"));
        }
        let reasoning = timeline
            .push(ItemKind::Reasoning, "summary\nhidden reasoning detail")
            .expect("reasoning id");
        let tool = timeline
            .push(ItemKind::Tool, "inspect completed\nhidden tool detail")
            .expect("tool id");
        assert!(!timeline.item(reasoning).expect("reasoning").expanded);
        assert!(!timeline.item(tool).expect("tool").expanded);

        let full = timeline.rows(32);
        let following = timeline.view(32, 7);
        assert!(following.pinned.is_some());
        assert_eq!(
            following.rows,
            full[following.start..following.start + following.rows.len()]
        );

        timeline.scroll_by(-6, 32, 7);
        let ViewportAnchor::Held(anchor) = timeline.viewport else {
            panic!("timeline must hold a content anchor");
        };
        let resized_full = timeline.rows(25);
        let resized = timeline.view(25, 6);
        assert_eq!(
            resized.rows,
            resized_full[resized.start..resized.start + resized.rows.len()]
        );
        assert_eq!(
            resized.rows.first().map(|row| row.item_id),
            Some(anchor.item_id)
        );
    }

    #[test]
    fn pin_capacity_is_consistent_at_every_next_prompt_boundary() {
        for response_items in 1..16 {
            for height in 3..9 {
                let mut timeline = Timeline::new();
                let _ = timeline.push(ItemKind::User, "first");
                for number in 0..response_items {
                    let _ = timeline.push(ItemKind::Assistant, format!("reply {number}"));
                }
                let _ = timeline.push(ItemKind::User, "next");
                let _ = timeline.push(ItemKind::Assistant, "latest");

                let view = timeline.view(30, height);
                let occupied = view.rows.len().saturating_add(if view.pinned.is_some() {
                    PinnedPrompt::ROWS
                } else {
                    0
                });
                assert!(
                    occupied <= usize::from(height),
                    "response={response_items}, height={height}"
                );
                let full = timeline.rows(30);
                assert_eq!(
                    view.rows,
                    full[view.start..view.start + view.rows.len()],
                    "response={response_items}, height={height}"
                );
                assert_eq!(view.rows.last(), full.last());
            }
        }
    }

    #[test]
    fn width_index_is_reused_until_a_geometry_mutation() {
        let mut timeline = Timeline::new();
        let id = timeline
            .push(ItemKind::Assistant, "cached layout")
            .expect("item id");
        let _ = timeline.view(40, 10);
        let first = timeline
            .layout_cache
            .borrow()
            .as_ref()
            .map(|cache| Arc::as_ptr(&cache.entries))
            .expect("layout cache");
        assert!(timeline.set_styles(
            id,
            vec![StyledRange {
                start_byte: 0,
                end_byte: 6,
                style: TextStyle::new(SemanticRole::Heading),
            }]
        ));
        let _ = timeline.view(40, 10);
        let second = timeline
            .layout_cache
            .borrow()
            .as_ref()
            .map(|cache| Arc::as_ptr(&cache.entries))
            .expect("layout cache");
        assert_eq!(first, second);

        assert!(timeline.append_text(id, " changed"));
        assert!(timeline.layout_cache.borrow().is_none());
    }

    #[test]
    fn held_timeline_marks_new_output_until_it_returns_to_bottom() {
        let mut timeline = Timeline::new();
        for number in 0..30 {
            let _ = timeline.push(ItemKind::Assistant, format!("item {number:02}"));
        }
        timeline.scroll_by(-12, 40, 8);
        let ViewportAnchor::Held(anchor) = timeline.viewport else {
            panic!("timeline must be held");
        };
        assert!(!timeline.has_new_content());

        let tail = timeline.items().last().expect("tail").id;
        assert!(timeline.append_text(tail, " streamed"));
        assert!(timeline.has_new_content());
        assert_eq!(timeline.viewport, ViewportAnchor::Held(anchor));
        let resized = timeline.view(31, 6);
        assert!(resized.new_content);
        assert!(timeline.has_new_content());
        assert_eq!(timeline.viewport, ViewportAnchor::Held(anchor));

        timeline.scroll_by(isize::MAX, 40, 8);
        assert_eq!(timeline.viewport, ViewportAnchor::FollowBottom);
        assert!(!timeline.has_new_content());

        timeline.scroll_by(-3, 40, 8);
        let _ = timeline.push(ItemKind::Notice, "later notice");
        assert!(timeline.has_new_content());
        timeline.follow_bottom();
        assert!(!timeline.has_new_content());
    }

    #[test]
    fn append_uses_constant_time_item_lookup_and_invalidates_only_its_wrap() {
        let mut timeline = Timeline::new();
        let first = timeline
            .push(ItemKind::Assistant, "first")
            .expect("first id");
        let second = timeline
            .push(ItemKind::Assistant, "second")
            .expect("second id");
        let _ = timeline.view(4, 2);
        assert_eq!(timeline.wrap_cache.borrow().len(), 2);
        assert!(timeline.append_text(first, " tail"));
        assert!(!timeline.wrap_cache.borrow().contains_key(&first));
        assert!(timeline.wrap_cache.borrow().contains_key(&second));
        assert_eq!(timeline.items[0].text, "first tail");
    }
}
