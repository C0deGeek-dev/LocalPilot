use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use localpilot_config::TimelineDensity;

use crate::sanitize_text;
use crate::text::{wrap_ranges, TextRow};

const COLLAPSED_TOOL_VISUAL_ROWS: usize = 4;
const FAILED_TOOL_VISUAL_ROWS: usize = 8;
const FAILED_TOOL_HEAD_ROWS: usize = 2;
const SUCCESS_GROUP_MIN_TOOLS: usize = 3;

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
    Question,
    Shell,
    Notice,
    Result,
}

/// Provider-neutral presentation stage for assistant prose. A segment begins as
/// an answer and becomes progress only when a later tool start proves it was
/// intermediate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssistantPresentation {
    Answer,
    Progress,
}

/// The honesty of a retained collaboration result, driving its distinct prefix and
/// colour. Success is only a genuine convergence; a bounded or aborted run is
/// incomplete; a failed run is an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultTone {
    Success,
    Incomplete,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityState {
    Running,
    Success,
    Error,
    Cancelled,
}

/// Source-versus-retained facts for one terminal tool result. The source is the
/// sanitized body delivered to the terminal event; the retained body is what
/// remains in the authoritative timeline after its independent view bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolPresentation {
    pub source_lines: usize,
    pub source_bytes: usize,
    pub retained_lines: usize,
    pub retained_bytes: usize,
    /// Completed runtime duration when the host supplied one.
    pub duration_ms: Option<u64>,
    pub terminal_truncated: bool,
    /// Byte boundary between the action headline and its metadata suffix.
    pub metadata_start: usize,
    /// Byte boundary at the end of the complete headline. Alignment is only
    /// safe when this boundary remains on the first projected visual row.
    pub metadata_end: usize,
}

/// Width-specific disclosure state derived from the authoritative wrap ranges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolDisclosure {
    pub expanded: bool,
    pub expandable: bool,
    /// Visual rows hidden by the compact projection at this width.
    pub hidden_visual_rows: usize,
    /// Index of the first tail row in the collapsed projection. When present,
    /// retained wrapped rows between the compact head and tail are hidden.
    pub tail_start_visual_row: Option<usize>,
    pub terminal_truncated: bool,
}

/// Projection-only summary of one consecutive successful tool run. Original
/// timeline items remain authoritative and reappear when the group expands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolGroupPresentation {
    pub tool_count: usize,
    pub duration_ms: Option<u64>,
    pub has_retained_details: bool,
    pub terminal_truncated: bool,
    pub expanded: bool,
}

/// One keyboard/pointer focus authority for original tools and projected
/// successful-run summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineFocusTarget {
    Tool(ItemId),
    SuccessGroup(ItemId),
}

impl TimelineFocusTarget {
    #[must_use]
    pub const fn item_id(self) -> ItemId {
        match self {
            Self::Tool(id) | Self::SuccessGroup(id) => id,
        }
    }
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
            ItemKind::Question => Self::Tool,
            ItemKind::Shell => Self::Tool,
            ItemKind::Notice => Self::Notice,
            ItemKind::Result => Self::Notice,
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
    pub assistant_presentation: Option<AssistantPresentation>,
    pub trailing: Option<String>,
    pub pending: bool,
    pub activity: Option<ActivityState>,
    pub tone: Option<ResultTone>,
    pub expanded: bool,
    pub tool: Option<ToolPresentation>,
}

impl TimelineItem {
    fn new(id: ItemId, kind: ItemKind, text: String) -> Self {
        let styles = full_range_style(&text, TextStyle::new(kind.into()));
        Self {
            id,
            kind,
            text,
            styles,
            assistant_presentation: (kind == ItemKind::Assistant)
                .then_some(AssistantPresentation::Answer),
            trailing: None,
            pending: false,
            activity: None,
            tone: None,
            expanded: !matches!(kind, ItemKind::Reasoning | ItemKind::Tool),
            tool: None,
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
    pub assistant_presentation: Option<AssistantPresentation>,
    pub part: VisualRowPart,
    pub trailing: Option<String>,
    pub pending: bool,
    pub activity: Option<ActivityState>,
    pub tone: Option<ResultTone>,
    pub tool: Option<ToolPresentation>,
    pub disclosure: Option<ToolDisclosure>,
    pub tool_group: Option<ToolGroupPresentation>,
    /// Retained wrapped rows skipped immediately before this projected row.
    /// This is projection metadata, never synthetic source text.
    pub omitted_before_visual_rows: usize,
    pub focused: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisualRowPart {
    Spacer,
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
    full_row_count: usize,
    tail_start_visual_row: Option<usize>,
}

#[derive(Debug, Clone, Copy)]
struct ItemLayout {
    start: usize,
    end: usize,
    leading_spacer: bool,
    kind: ItemLayoutKind,
}

#[derive(Debug, Clone, Copy)]
enum ItemLayoutKind {
    Item,
    SuccessGroupHead(ToolGroupPresentation),
    CollapsedSuccessGroupMember { head: ItemId },
}

#[derive(Debug, Clone)]
struct CachedLayout {
    width: u16,
    entries: Arc<[ItemLayout]>,
    total_rows: usize,
}

#[derive(Debug, Clone, Copy)]
struct SuccessGroup {
    head_index: usize,
    end_index: usize,
    presentation: ToolGroupPresentation,
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
    /// When false, `ItemKind::Reasoning` items are hidden from render, scroll
    /// geometry, search, selection, and new-content notification — the raw items
    /// are retained (streaming continues; the print path drops them separately).
    reasoning_visible: bool,
    density: TimelineDensity,
    group_successful_tools: bool,
    expanded_success_groups: HashSet<ItemId>,
    focused_target: Option<TimelineFocusTarget>,
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
            reasoning_visible: true,
            density: TimelineDensity::Compact,
            group_successful_tools: false,
            expanded_success_groups: HashSet::new(),
            focused_target: None,
        }
    }

    #[must_use]
    pub fn items(&self) -> &[TimelineItem] {
        &self.items
    }

    /// Whether reasoning items are currently shown.
    #[must_use]
    pub const fn reasoning_visible(&self) -> bool {
        self.reasoning_visible
    }

    #[must_use]
    pub const fn density(&self) -> TimelineDensity {
        self.density
    }

    pub fn set_density(&mut self, density: TimelineDensity) {
        if self.density != density {
            self.density = density;
            self.invalidate_layout();
        }
    }

    #[must_use]
    pub const fn group_successful_tools(&self) -> bool {
        self.group_successful_tools
    }

    pub fn set_group_successful_tools(&mut self, enabled: bool) {
        if self.group_successful_tools != enabled {
            self.group_successful_tools = enabled;
            self.focused_target = None;
            self.invalidate_layout();
        }
    }

    #[must_use]
    pub const fn focused_target(&self) -> Option<TimelineFocusTarget> {
        self.focused_target
    }

    #[must_use]
    pub fn focused_tool(&self) -> Option<ItemId> {
        match self.focused_target {
            Some(TimelineFocusTarget::Tool(id)) => Some(id),
            Some(TimelineFocusTarget::SuccessGroup(_)) | None => None,
        }
    }

    pub fn clear_tool_focus(&mut self) {
        self.focused_target = None;
    }

    /// Set reasoning visibility. Invalidates the layout cache so scroll geometry
    /// is recomputed with the new set of visible rows.
    pub fn set_reasoning_visible(&mut self, visible: bool) {
        if self.reasoning_visible != visible {
            self.reasoning_visible = visible;
            self.invalidate_layout();
        }
    }

    /// The single visibility authority: whether an item of this kind is currently
    /// hidden. Every render/layout/search/selection/new-content path routes
    /// through this, so a future visibility rule cannot drift between them.
    fn kind_hidden(&self, kind: ItemKind) -> bool {
        !self.reasoning_visible && kind == ItemKind::Reasoning
    }

    /// Whether `item` is currently hidden. Hidden items stay in `self.items`
    /// (index-stable) but contribute no rows/geometry.
    fn is_reasoning_hidden(&self, item: &TimelineItem) -> bool {
        self.kind_hidden(item.kind)
    }

    /// Whether `item` currently occupies a row — the inverse of the hidden
    /// authority. `pub(crate)` so out-of-module search/selection use the same rule
    /// (raw `items()` stays raw).
    #[must_use]
    pub(crate) fn item_is_visible(&self, item: &TimelineItem) -> bool {
        !self.kind_hidden(item.kind)
    }

    /// Note new content below a held viewport, unless the appended item is a
    /// currently-hidden item (which has no visible row).
    fn note_new_content(&self, kind: ItemKind) {
        if !self.kind_hidden(kind) && !matches!(self.viewport, ViewportAnchor::FollowBottom) {
            self.new_content.set(true);
        }
    }

    #[must_use]
    pub fn item(&self, id: ItemId) -> Option<&TimelineItem> {
        self.item_positions
            .get(&id)
            .and_then(|index| self.items.get(*index))
    }

    fn success_groups(&self) -> Vec<SuccessGroup> {
        if !self.group_successful_tools {
            return Vec::new();
        }
        let mut groups = Vec::new();
        let mut index = 0usize;
        while index < self.items.len() {
            if !is_groupable_success(&self.items[index]) {
                index += 1;
                continue;
            }
            let head_index = index;
            while index < self.items.len() && is_groupable_success(&self.items[index]) {
                index += 1;
            }
            let end_index = index;
            let tool_count = end_index.saturating_sub(head_index);
            if tool_count < SUCCESS_GROUP_MIN_TOOLS {
                continue;
            }
            let members = &self.items[head_index..end_index];
            let duration_ms = members.iter().try_fold(0u64, |total, item| {
                item.tool?
                    .duration_ms
                    .map(|duration| total.saturating_add(duration))
            });
            let head = self.items[head_index].id;
            groups.push(SuccessGroup {
                head_index,
                end_index,
                presentation: ToolGroupPresentation {
                    tool_count,
                    duration_ms,
                    has_retained_details: members.iter().any(|item| item.text.contains('\n')),
                    terminal_truncated: members
                        .iter()
                        .any(|item| item.tool.is_some_and(|tool| tool.terminal_truncated)),
                    expanded: self.expanded_success_groups.contains(&head),
                },
            });
        }
        groups
    }

    fn success_group_containing(&self, id: ItemId) -> Option<SuccessGroup> {
        let index = self.item_positions.get(&id).copied()?;
        self.success_groups()
            .into_iter()
            .find(|group| group.head_index <= index && index < group.end_index)
    }

    /// Reveal original tools inside the containing successful run before a
    /// search/anchor operation targets their source bytes.
    pub fn expand_success_group_containing(&mut self, id: ItemId) -> bool {
        let Some(group) = self.success_group_containing(id) else {
            return false;
        };
        let head = self.items[group.head_index].id;
        if self.expanded_success_groups.insert(head) {
            self.invalidate_layout();
        }
        true
    }

    pub fn push(&mut self, kind: ItemKind, text: impl Into<String>) -> Option<ItemId> {
        let raw = self.next_id?;
        let id = ItemId(raw);
        self.next_id = raw.checked_add(1);
        let text = sanitize_text(&text.into());
        self.item_positions.insert(id, self.items.len());
        self.items.push(TimelineItem::new(id, kind, text));
        self.note_new_content(kind);
        self.invalidate_layout();
        Some(id)
    }

    /// Append a retained result item, colouring its whole body by the result tone so
    /// a non-success result never reads as ordinary output.
    pub fn push_result(&mut self, text: impl Into<String>, tone: ResultTone) -> Option<ItemId> {
        let id = self.push(ItemKind::Result, text)?;
        if let Some(index) = self.item_positions.get(&id).copied() {
            let role = match tone {
                ResultTone::Success => SemanticRole::Success,
                ResultTone::Incomplete => SemanticRole::Accent,
                ResultTone::Error => SemanticRole::Error,
            };
            let item = &mut self.items[index];
            item.tone = Some(tone);
            item.styles = full_range_style(&item.text, TextStyle::new(role));
        }
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
        self.note_new_content(self.items[index].kind);
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
        self.note_new_content(self.items[index].kind);
        self.invalidate_layout();
        true
    }

    pub fn replace_text(&mut self, id: ItemId, text: impl Into<String>) -> bool {
        let Some(index) = self.item_positions.get(&id).copied() else {
            return false;
        };
        let item = &mut self.items[index];
        item.text = sanitize_text(&text.into());
        item.styles = full_range_style(&item.text, TextStyle::new(item.kind.into()));
        self.wrap_cache.borrow_mut().remove(&id);
        self.note_new_content(self.items[index].kind);
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
        if self.items[index].activity == activity {
            return true;
        }
        self.items[index].activity = activity;
        self.wrap_cache.borrow_mut().remove(&id);
        self.invalidate_layout();
        true
    }

    /// Reclassify assistant presentation without changing kind, text, bytes, or
    /// geometry. Non-assistant items reject the update.
    pub fn set_assistant_presentation(
        &mut self,
        id: ItemId,
        presentation: AssistantPresentation,
    ) -> bool {
        let Some(index) = self.item_positions.get(&id).copied() else {
            return false;
        };
        let item = &mut self.items[index];
        if item.kind != ItemKind::Assistant {
            return false;
        }
        item.assistant_presentation = Some(presentation);
        true
    }

    pub fn set_tool_presentation(&mut self, id: ItemId, presentation: ToolPresentation) -> bool {
        let Some(index) = self.item_positions.get(&id).copied() else {
            return false;
        };
        if self.items[index].kind != ItemKind::Tool {
            return false;
        }
        self.items[index].tool = Some(presentation);
        self.wrap_cache.borrow_mut().remove(&id);
        self.invalidate_layout();
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

    pub fn toggle_expandable(&mut self, id: ItemId) -> bool {
        let Some(index) = self.item_positions.get(&id).copied() else {
            return false;
        };
        let item = &self.items[index];
        if !matches!(item.kind, ItemKind::Reasoning | ItemKind::Tool) || !item.text.contains('\n') {
            return false;
        }
        let expanded = !item.expanded;
        self.items[index].expanded = expanded;
        self.wrap_cache.borrow_mut().remove(&id);
        self.invalidate_layout();
        true
    }

    /// Focus one original tool or one projected successful-run summary.
    pub fn focus_target(&mut self, target: TimelineFocusTarget, width: u16, height: u16) -> bool {
        let id = target.item_id();
        let Some(index) = self.item_positions.get(&id).copied() else {
            return false;
        };
        let Some(item) = self.items.get(index) else {
            return false;
        };
        if item.kind != ItemKind::Tool {
            return false;
        }
        match target {
            TimelineFocusTarget::Tool(_) => {
                if self
                    .success_group_containing(id)
                    .is_some_and(|group| !group.presentation.expanded)
                {
                    return false;
                }
            }
            TimelineFocusTarget::SuccessGroup(_) => {
                if !self
                    .success_group_containing(id)
                    .is_some_and(|group| group.head_index == index)
                {
                    return false;
                }
            }
        }
        self.focused_target = Some(target);
        self.reveal_focus_target(target, width, height);
        true
    }

    /// Compatibility helper for callers that explicitly target an original tool.
    pub fn focus_tool(&mut self, id: ItemId, width: u16, height: u16) -> bool {
        self.focus_target(TimelineFocusTarget::Tool(id), width, height)
    }

    /// Move the stable tool cursor in timeline reading order. The first move
    /// starts at the nearest edge and subsequent moves clamp at either end.
    pub fn move_tool_focus(&mut self, forward: bool, width: u16, height: u16) -> bool {
        let targets = self.focus_targets();
        if targets.is_empty() {
            self.focused_target = None;
            return false;
        }
        let current = self
            .focused_target
            .and_then(|focused| targets.iter().position(|target| *target == focused));
        let next = match (current, forward) {
            (Some(index), true) => index.saturating_add(1).min(targets.len().saturating_sub(1)),
            (Some(index), false) => index.saturating_sub(1),
            (None, true) => 0,
            (None, false) => targets.len().saturating_sub(1),
        };
        self.focus_target(targets[next], width, height)
    }

    /// Toggle the focused tool while preserving the pre-toggle viewport start.
    /// The only movement permitted is deterministic clamping when the shorter
    /// post-toggle timeline no longer has that start row.
    pub fn toggle_focused_tool(&mut self, width: u16, height: u16) -> bool {
        let Some(target) = self.focused_target else {
            return false;
        };
        match target {
            TimelineFocusTarget::Tool(id) => self.toggle_tool_preserving_view(id, width, height),
            TimelineFocusTarget::SuccessGroup(id) => {
                self.toggle_success_group_preserving_view(id, width, height)
            }
        }
    }

    fn focus_targets(&self) -> Vec<TimelineFocusTarget> {
        let groups = self.success_groups();
        let mut targets = Vec::new();
        let mut index = 0usize;
        for group in groups {
            while index < group.head_index {
                if self.items[index].kind == ItemKind::Tool {
                    targets.push(TimelineFocusTarget::Tool(self.items[index].id));
                }
                index += 1;
            }
            let head = self.items[group.head_index].id;
            targets.push(TimelineFocusTarget::SuccessGroup(head));
            if group.presentation.expanded {
                targets.extend(
                    self.items[group.head_index..group.end_index]
                        .iter()
                        .map(|item| TimelineFocusTarget::Tool(item.id)),
                );
            }
            index = group.end_index;
        }
        while index < self.items.len() {
            if self.items[index].kind == ItemKind::Tool {
                targets.push(TimelineFocusTarget::Tool(self.items[index].id));
            }
            index += 1;
        }
        targets
    }

    fn toggle_success_group_preserving_view(
        &mut self,
        head: ItemId,
        width: u16,
        height: u16,
    ) -> bool {
        let Some(group) = self.success_group_containing(head) else {
            return false;
        };
        if self.items[group.head_index].id != head {
            return false;
        }
        let before = self.view(width, height);
        if group.presentation.expanded {
            self.expanded_success_groups.remove(&head);
        } else {
            self.expanded_success_groups.insert(head);
        }
        self.invalidate_layout();
        let after = self.view(width, height);
        let max_start = self.total_rows(width).saturating_sub(after.viewport_rows);
        self.set_viewport_start(before.start.min(max_start), width, max_start);
        true
    }

    fn toggle_tool_preserving_view(&mut self, id: ItemId, width: u16, height: u16) -> bool {
        let before = self.view(width, height);
        if !self.toggle_expandable(id) {
            return false;
        }
        let after = self.view(width, height);
        let max_start = self.total_rows(width).saturating_sub(after.viewport_rows);
        self.set_viewport_start(before.start.min(max_start), width, max_start);
        true
    }

    fn reveal_focus_target(&mut self, target: TimelineFocusTarget, width: u16, height: u16) {
        let id = target.item_id();
        let Some(index) = self.item_positions.get(&id).copied() else {
            return;
        };
        let layout = self.layout_index(width);
        let Some(entry) = layout.entries.get(index) else {
            return;
        };
        let group_header_rows = usize::from(matches!(
            (target, entry.kind),
            (
                TimelineFocusTarget::Tool(_),
                ItemLayoutKind::SuccessGroupHead(ToolGroupPresentation { expanded: true, .. })
            )
        ));
        let headline = entry
            .start
            .saturating_add(usize::from(entry.leading_spacer))
            .saturating_add(group_header_rows);
        let view = self.view(width, height);
        let end = view.start.saturating_add(view.viewport_rows);
        let start = if headline < view.start {
            headline
        } else if headline >= end {
            headline
                .saturating_add(1)
                .saturating_sub(view.viewport_rows)
        } else {
            return;
        };
        let max_start = layout.total_rows.saturating_sub(view.viewport_rows);
        self.set_viewport_start(start.min(max_start), width, max_start);
    }

    #[must_use]
    pub fn rows(&self, width: u16) -> Vec<VisualRow> {
        let end = self.total_rows(width);
        self.project_rows(width, 0, end)
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
        let mut collapsed_group_by_index = HashMap::new();
        for group in self
            .success_groups()
            .into_iter()
            .filter(|group| !group.presentation.expanded)
        {
            for index in group.head_index..group.end_index {
                collapsed_group_by_index.insert(index, group);
            }
        }
        let mut emitted_collapsed_groups = HashSet::new();
        for (index, item) in self.items[start_index..=end_index].iter().enumerate() {
            let index = start_index + index;
            // A hidden reasoning item has no visible row, so a selection spanning
            // it must not splice its text into the copy.
            if self.is_reasoning_hidden(item) {
                continue;
            }
            if let Some(group) = collapsed_group_by_index.get(&index).copied() {
                let head = self.items[group.head_index].id;
                if emitted_collapsed_groups.insert(head) {
                    let selected_tools = end_index
                        .saturating_add(1)
                        .min(group.end_index)
                        .saturating_sub(start_index.max(group.head_index));
                    if selected_any {
                        output.push('\n');
                    }
                    output.push_str(&format!("… {} successful tools grouped …", selected_tools));
                    selected_any = true;
                }
                continue;
            }
            let from = if index == start_index {
                start_byte.min(item.text.len())
            } else {
                0
            };
            let to = if index == end_index {
                end_byte.min(item.text.len())
            } else {
                item.text.len()
            };
            if from > to || !item.text.is_char_boundary(from) || !item.text.is_char_boundary(to) {
                continue;
            }
            let (segments, omitted_rows) = self.displayed_segments(item);
            let mut selected_in_item = false;
            for (segment_start, segment_end) in segments {
                let segment_from = from.max(segment_start);
                let segment_to = to.min(segment_end);
                if segment_from >= segment_to
                    || !item.text.is_char_boundary(segment_from)
                    || !item.text.is_char_boundary(segment_to)
                {
                    continue;
                }
                if selected_any {
                    if selected_in_item && omitted_rows > 0 {
                        let row_word = if omitted_rows == 1 { "row" } else { "rows" };
                        output.push_str(&format!(
                            "\n… {omitted_rows} earlier wrapped {row_word} hidden …\n"
                        ));
                    } else {
                        output.push('\n');
                    }
                }
                output.push_str(&item.text[segment_from..segment_to]);
                selected_any = true;
                selected_in_item = true;
            }
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
        let display_text = if item.kind == ItemKind::Tool && !item.expanded {
            item.text.as_str()
        } else {
            displayed_text(item)
        };
        if let Some(cached) = self.wrap_cache.borrow().get(&item.id) {
            if cached.width == width && cached.text_len == display_text.len() {
                return Arc::clone(&cached.ranges);
            }
        }
        let mut ranges = wrap_ranges(display_text, width);
        let full_row_count = ranges.len();
        let mut tail_start_visual_row = None;
        if item.kind == ItemKind::Tool && !item.expanded {
            if item.activity == Some(ActivityState::Error)
                && full_row_count > FAILED_TOOL_VISUAL_ROWS
            {
                let tail_rows = FAILED_TOOL_VISUAL_ROWS.saturating_sub(FAILED_TOOL_HEAD_ROWS);
                let tail_start = full_row_count.saturating_sub(tail_rows);
                let tail = ranges[tail_start..].to_vec();
                ranges.truncate(FAILED_TOOL_HEAD_ROWS);
                tail_start_visual_row = Some(ranges.len());
                ranges.extend(tail);
            } else {
                ranges.truncate(if item.activity == Some(ActivityState::Error) {
                    FAILED_TOOL_VISUAL_ROWS
                } else {
                    COLLAPSED_TOOL_VISUAL_ROWS
                });
            }
        }
        let ranges: Arc<[TextRow]> = ranges.into();
        self.wrap_cache.borrow_mut().insert(
            item.id,
            CachedWrap {
                width,
                text_len: display_text.len(),
                ranges: Arc::clone(&ranges),
                full_row_count,
                tail_start_visual_row,
            },
        );
        ranges
    }

    fn tool_disclosure(&self, item: &TimelineItem) -> Option<ToolDisclosure> {
        let presentation = item.tool?;
        let cached = self.wrap_cache.borrow();
        let cached = cached.get(&item.id)?;
        let failed = item.activity == Some(ActivityState::Error);
        let collapsed_budget = if failed {
            FAILED_TOOL_VISUAL_ROWS
        } else {
            COLLAPSED_TOOL_VISUAL_ROWS
        };
        let hidden_visual_rows = cached.full_row_count.saturating_sub(collapsed_budget);
        Some(ToolDisclosure {
            expanded: item.expanded,
            expandable: hidden_visual_rows > 0,
            hidden_visual_rows,
            tail_start_visual_row: (failed && hidden_visual_rows > 0)
                .then_some(FAILED_TOOL_HEAD_ROWS),
            terminal_truncated: presentation.terminal_truncated,
        })
    }

    fn displayed_segments(&self, item: &TimelineItem) -> (Vec<(usize, usize)>, usize) {
        if item.kind != ItemKind::Tool || item.expanded {
            return (vec![(0, displayed_text(item).len())], 0);
        }
        let cached = self.wrap_cache.borrow();
        let Some(cached) = cached
            .get(&item.id)
            .filter(|cached| cached.text_len == item.text.len())
        else {
            return (vec![(0, displayed_text(item).len())], 0);
        };
        let Some(last) = cached.ranges.last() else {
            return (Vec::new(), 0);
        };
        let Some(tail_start) = cached.tail_start_visual_row else {
            return (vec![(0, last.end_byte)], 0);
        };
        let Some(head) = tail_start
            .checked_sub(1)
            .and_then(|index| cached.ranges.get(index))
        else {
            return (vec![(0, last.end_byte)], 0);
        };
        let Some(tail) = cached.ranges.get(tail_start) else {
            return (vec![(0, last.end_byte)], 0);
        };
        (
            vec![(0, head.end_byte), (tail.start_byte, last.end_byte)],
            cached.full_row_count.saturating_sub(cached.ranges.len()),
        )
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
        // A hidden reasoning item is zero-height: a held anchor pointing inside it
        // must collapse to its boundary, never a wrapped-row offset that would land
        // in a later visible item or past the total.
        if self.is_reasoning_hidden(item) {
            return entry.start.min(layout.total_rows.saturating_sub(1));
        }
        if let ItemLayoutKind::CollapsedSuccessGroupMember { head } = entry.kind {
            return self
                .item_positions
                .get(&head)
                .and_then(|head_index| layout.entries.get(*head_index))
                .map_or(entry.start, |head_entry| {
                    head_entry
                        .start
                        .saturating_add(usize::from(head_entry.leading_spacer))
                });
        }
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
        let group_header_rows = usize::from(matches!(
            entry.kind,
            ItemLayoutKind::SuccessGroupHead(ToolGroupPresentation { expanded: true, .. })
        ));
        entry
            .start
            .saturating_add(usize::from(entry.leading_spacer))
            .saturating_add(group_header_rows)
            .saturating_add(frame_prefix_rows(item.kind))
            .saturating_add(within)
    }

    fn point_at_row(&self, width: u16, target: usize) -> Option<ContentPoint> {
        let layout = self.layout_index(width);
        let index = layout.entries.partition_point(|entry| entry.end <= target);
        let entry = layout.entries.get(index)?;
        let item = self.items.get(index)?;
        let local = target.saturating_sub(entry.start);
        let spacer = usize::from(entry.leading_spacer);
        if local < spacer {
            return Some(ContentPoint {
                item_id: item.id,
                byte: 0,
            });
        }
        let local = local.saturating_sub(spacer);
        if let ItemLayoutKind::SuccessGroupHead(group) = entry.kind {
            if local == 0 || !group.expanded {
                return Some(ContentPoint {
                    item_id: item.id,
                    byte: 0,
                });
            }
        }
        let local = local.saturating_sub(usize::from(matches!(
            entry.kind,
            ItemLayoutKind::SuccessGroupHead(ToolGroupPresentation { expanded: true, .. })
        )));
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
            if entry.start >= end {
                break;
            }
            if matches!(
                entry.kind,
                ItemLayoutKind::CollapsedSuccessGroupMember { .. }
            ) {
                continue;
            }
            let ranges = self.row_ranges(item, item_content_width(item, width));
            let disclosure = self.tool_disclosure(item);
            let item_rows = entry.end.saturating_sub(entry.start);
            let first = start.saturating_sub(entry.start);
            let last = end.saturating_sub(entry.start).min(item_rows);
            match entry.kind {
                ItemLayoutKind::Item => output.extend(project_item_rows(
                    item,
                    &ranges,
                    first,
                    last,
                    entry.leading_spacer,
                    disclosure,
                    self.focused_target == Some(TimelineFocusTarget::Tool(item.id)),
                )),
                ItemLayoutKind::SuccessGroupHead(group) => {
                    output.extend(project_success_group_rows(
                        item,
                        &ranges,
                        first,
                        last,
                        entry.leading_spacer,
                        disclosure,
                        SuccessGroupProjection {
                            group,
                            focused_target: self.focused_target,
                        },
                    ));
                }
                ItemLayoutKind::CollapsedSuccessGroupMember { .. } => {}
            }
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
        let mut previous_visible_kind = None;
        let groups = self.success_groups();
        let group_heads = groups
            .iter()
            .map(|group| (group.head_index, *group))
            .collect::<HashMap<_, _>>();
        let mut collapsed_members = HashMap::new();
        for group in groups.iter().filter(|group| !group.presentation.expanded) {
            let head = self.items[group.head_index].id;
            for index in group.head_index + 1..group.end_index {
                collapsed_members.insert(index, head);
            }
        }
        let mut entry_values = Vec::with_capacity(self.items.len());
        for (index, item) in self.items.iter().enumerate() {
            if let Some(head) = collapsed_members.get(&index).copied() {
                entry_values.push(ItemLayout {
                    start,
                    end: start,
                    leading_spacer: false,
                    kind: ItemLayoutKind::CollapsedSuccessGroupMember { head },
                });
                continue;
            }
            // A hidden reasoning item stays in the entries vector (index
            // identity with `self.items`) but is zero-height.
            let hidden = self.is_reasoning_hidden(item);
            let required_spacer = previous_visible_kind == Some(ItemKind::Tool)
                && matches!(item.kind, ItemKind::Assistant | ItemKind::Reasoning);
            let density_spacer = self.density.includes_optional_spacers()
                && previous_visible_kind == Some(ItemKind::Tool)
                && item.kind == ItemKind::Tool;
            let leading_spacer = !hidden && (required_spacer || density_spacer);
            let (height, kind) = if hidden {
                (0, ItemLayoutKind::Item)
            } else if let Some(group) = group_heads.get(&index).copied() {
                let original_rows = if group.presentation.expanded {
                    self.row_ranges(item, item_content_width(item, width)).len()
                } else {
                    0
                };
                previous_visible_kind = Some(ItemKind::Tool);
                (
                    usize::from(leading_spacer)
                        .saturating_add(1)
                        .saturating_add(original_rows),
                    ItemLayoutKind::SuccessGroupHead(group.presentation),
                )
            } else {
                let ranges = self.row_ranges(item, item_content_width(item, width));
                let height = item_visual_row_count(item, &ranges, leading_spacer);
                previous_visible_kind = Some(item.kind);
                (height, ItemLayoutKind::Item)
            };
            let end = start.saturating_add(height);
            entry_values.push(ItemLayout {
                start,
                end,
                leading_spacer,
                kind,
            });
            start = end;
        }
        let entries: Arc<[ItemLayout]> = entry_values.into();
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

fn is_groupable_success(item: &TimelineItem) -> bool {
    item.kind == ItemKind::Tool && item.activity == Some(ActivityState::Success)
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
        ItemKind::Tool => 3,
        ItemKind::Assistant
        | ItemKind::Reasoning
        | ItemKind::Question
        | ItemKind::Shell
        | ItemKind::Notice
        | ItemKind::Result => 2,
    };
    width.saturating_sub(decoration).max(1)
}

fn item_visual_row_count(item: &TimelineItem, ranges: &[TextRow], leading_spacer: bool) -> usize {
    ranges
        .len()
        .saturating_add(usize::from(leading_spacer))
        .saturating_add(if item.kind == ItemKind::User { 2 } else { 0 })
}

fn project_item_rows(
    item: &TimelineItem,
    ranges: &[TextRow],
    start: usize,
    end: usize,
    leading_spacer: bool,
    disclosure: Option<ToolDisclosure>,
    focused: bool,
) -> Vec<VisualRow> {
    let mut output = Vec::with_capacity(end.saturating_sub(start));
    let prefix = frame_prefix_rows(item.kind);
    let spacer = usize::from(leading_spacer);
    for index in start..end {
        if leading_spacer && index == 0 {
            output.push(frame_row(item, VisualRowPart::Spacer, focused));
            continue;
        }
        let item_index = index.saturating_sub(spacer);
        if item.kind == ItemKind::User && item_index == 0 {
            output.push(frame_row(item, VisualRowPart::FrameTop, focused));
            continue;
        }
        let content_index = item_index.saturating_sub(prefix);
        if let Some(range) = ranges.get(content_index) {
            let omitted_before_visual_rows = disclosure
                .filter(|value| {
                    !value.expanded && value.tail_start_visual_row == Some(content_index)
                })
                .map_or(0, |value| value.hidden_visual_rows);
            output.push(visual_row(
                item,
                *range,
                VisualRowPart::Content {
                    first: content_index == 0,
                    last: content_index + 1 == ranges.len(),
                },
                disclosure,
                omitted_before_visual_rows,
                focused,
            ));
        } else if item.kind == ItemKind::User {
            output.push(frame_row(item, VisualRowPart::FrameBottom, focused));
        }
    }
    output
}

#[derive(Clone, Copy)]
struct SuccessGroupProjection {
    group: ToolGroupPresentation,
    focused_target: Option<TimelineFocusTarget>,
}

fn project_success_group_rows(
    item: &TimelineItem,
    ranges: &[TextRow],
    start: usize,
    end: usize,
    leading_spacer: bool,
    disclosure: Option<ToolDisclosure>,
    projection: SuccessGroupProjection,
) -> Vec<VisualRow> {
    let mut output = Vec::with_capacity(end.saturating_sub(start));
    let spacer = usize::from(leading_spacer);
    for index in start..end {
        if leading_spacer && index == 0 {
            output.push(frame_row(item, VisualRowPart::Spacer, false));
            continue;
        }
        let local = index.saturating_sub(spacer);
        if local == 0 {
            output.push(success_group_row(
                item,
                projection.group,
                projection.focused_target == Some(TimelineFocusTarget::SuccessGroup(item.id)),
            ));
            continue;
        }
        if !projection.group.expanded {
            continue;
        }
        let content_index = local.saturating_sub(1);
        if let Some(range) = ranges.get(content_index) {
            let omitted_before_visual_rows = disclosure
                .filter(|value| {
                    !value.expanded && value.tail_start_visual_row == Some(content_index)
                })
                .map_or(0, |value| value.hidden_visual_rows);
            output.push(visual_row(
                item,
                *range,
                VisualRowPart::Content {
                    first: content_index == 0,
                    last: content_index + 1 == ranges.len(),
                },
                disclosure,
                omitted_before_visual_rows,
                projection.focused_target == Some(TimelineFocusTarget::Tool(item.id)),
            ));
        }
    }
    output
}

fn success_group_row(
    item: &TimelineItem,
    group: ToolGroupPresentation,
    focused: bool,
) -> VisualRow {
    let mut text = format!("{} tools completed", group.tool_count);
    if let Some(duration_ms) = group.duration_ms {
        text.push_str(&format!(" · {duration_ms} ms"));
    }
    if group.has_retained_details {
        text.push_str(if group.expanded {
            " · retained details shown"
        } else {
            " · retained details hidden"
        });
    }
    if group.terminal_truncated {
        text.push_str(" · terminal view truncated");
    }
    VisualRow {
        item_id: item.id,
        kind: ItemKind::Tool,
        start_byte: 0,
        end_byte: 0,
        text,
        spans: Vec::new(),
        assistant_presentation: None,
        part: VisualRowPart::Content {
            first: true,
            last: true,
        },
        trailing: None,
        pending: false,
        activity: Some(ActivityState::Success),
        tone: None,
        tool: None,
        disclosure: None,
        tool_group: Some(group),
        omitted_before_visual_rows: 0,
        focused,
    }
}

fn frame_row(item: &TimelineItem, part: VisualRowPart, focused: bool) -> VisualRow {
    let byte = if part == VisualRowPart::FrameBottom {
        item.text.len()
    } else {
        0
    };
    VisualRow {
        item_id: item.id,
        kind: item.kind,
        start_byte: byte,
        end_byte: byte,
        text: String::new(),
        spans: Vec::new(),
        assistant_presentation: item.assistant_presentation,
        part,
        trailing: None,
        pending: item.pending,
        activity: item.activity,
        tone: item.tone,
        tool: item.tool,
        disclosure: None,
        tool_group: None,
        omitted_before_visual_rows: 0,
        focused,
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

fn visual_row(
    item: &TimelineItem,
    range: TextRow,
    part: VisualRowPart,
    disclosure: Option<ToolDisclosure>,
    omitted_before_visual_rows: usize,
    focused: bool,
) -> VisualRow {
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
        assistant_presentation: item.assistant_presentation,
        part,
        trailing: matches!(part, VisualRowPart::Content { first: true, .. })
            .then(|| item.trailing.clone())
            .flatten(),
        pending: item.pending,
        activity: item.activity,
        tone: item.tone,
        tool: item.tool,
        disclosure,
        tool_group: None,
        omitted_before_visual_rows,
        focused,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_successful_tool(
        timeline: &mut Timeline,
        name: &str,
        body: &str,
        duration_ms: Option<u64>,
    ) -> ItemId {
        let mut text = format!("{name} completed");
        let metadata_start = name.len();
        let metadata_end = text.len();
        if !body.is_empty() {
            text.push('\n');
            text.push_str(body);
        }
        let id = timeline.push(ItemKind::Tool, &text).expect("tool");
        assert!(timeline.set_tool_presentation(
            id,
            ToolPresentation {
                source_lines: body.lines().count(),
                source_bytes: body.len(),
                retained_lines: body.lines().count(),
                retained_bytes: body.len(),
                duration_ms,
                terminal_truncated: false,
                metadata_start,
                metadata_end,
            },
        ));
        assert!(timeline.set_activity(id, Some(ActivityState::Success)));
        id
    }

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
    fn density_changes_only_optional_tool_run_spacing() {
        let mut timeline = Timeline::new();
        let opening = timeline
            .push(ItemKind::Assistant, "I will inspect the files")
            .expect("opening narration");
        let first_tool = timeline
            .push(ItemKind::Tool, "Read manifest")
            .expect("tool");
        let second_tool = timeline.push(ItemKind::Tool, "Read log").expect("tool");
        let narration = timeline
            .push(ItemKind::Assistant, "I found the issue")
            .expect("narration");

        timeline.set_density(TimelineDensity::Comfortable);
        let comfortable = timeline.rows(40);
        assert_eq!(comfortable.len(), 6);
        assert_eq!(comfortable[0].item_id, opening);
        assert_eq!(comfortable[1].item_id, first_tool);
        assert_eq!(comfortable[2].part, VisualRowPart::Spacer);
        assert_eq!(comfortable[3].item_id, second_tool);
        assert_eq!(comfortable[4].part, VisualRowPart::Spacer);
        assert_eq!(comfortable[5].item_id, narration);

        timeline.set_density(TimelineDensity::Compact);
        let compact = timeline.rows(40);
        assert_eq!(compact.len(), 5);
        assert_eq!(compact[0].item_id, opening);
        assert_eq!(compact[1].item_id, first_tool);
        assert_eq!(compact[2].item_id, second_tool);
        assert_eq!(compact[3].part, VisualRowPart::Spacer);
        assert_eq!(compact[4].item_id, narration);
        assert_eq!(timeline.view(40, 10).total_rows, compact.len());

        timeline.start_selection(ContentPoint {
            item_id: first_tool,
            byte: 0,
        });
        timeline.extend_selection(ContentPoint {
            item_id: narration,
            byte: "I found the issue".len(),
        });
        assert_eq!(
            timeline.selected_text().as_deref(),
            Some("Read manifest\nRead log\nI found the issue")
        );
    }

    #[test]
    fn hidden_reasoning_keeps_one_tool_to_narration_spacer() {
        let mut timeline = Timeline::new();
        let _ = timeline.push(ItemKind::Tool, "Read manifest");
        let reasoning = timeline
            .push(ItemKind::Reasoning, "Checking the result")
            .expect("reasoning");
        let assistant = timeline
            .push(ItemKind::Assistant, "The result is valid")
            .expect("assistant");

        let visible = timeline.rows(40);
        assert_eq!(visible.len(), 4);
        assert_eq!(visible[1].part, VisualRowPart::Spacer);
        assert_eq!(visible[2].item_id, reasoning);
        assert_eq!(visible[3].item_id, assistant);

        timeline.set_reasoning_visible(false);
        let hidden = timeline.rows(40);
        assert_eq!(hidden.len(), 3);
        assert_eq!(hidden[1].part, VisualRowPart::Spacer);
        assert_eq!(hidden[2].item_id, assistant);
    }

    #[test]
    fn activity_details_expand_in_place_without_a_second_timeline_item() {
        let mut timeline = Timeline::new();
        let tool = timeline
            .push(ItemKind::Tool, "read_file completed\nline one\nline two")
            .expect("tool id");
        assert!(timeline.set_activity(tool, Some(ActivityState::Success)));
        let collapsed = timeline.rows(40);
        assert_eq!(collapsed.len(), 3);
        assert_eq!(collapsed[0].text, "read_file completed");
        assert_eq!(collapsed[2].text, "line two");
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
            Some("read_file completed\nline one\nline two")
        );

        assert!(timeline.set_expanded(tool, true));
        let expanded = timeline.rows(40);
        assert_eq!(expanded.len(), 3);
        assert_eq!(expanded[2].text, "line two");
        assert_eq!(timeline.items().len(), 1);
        assert!(timeline.toggle_expandable(tool));
        assert!(!timeline.item(tool).expect("tool").expanded);
        assert!(timeline.toggle_expandable(tool));
        assert!(timeline.item(tool).expect("tool").expanded);
    }

    #[test]
    fn collapsed_tool_preview_is_four_wrapped_rows_and_copy_stays_visible() {
        let mut timeline = Timeline::new();
        let tool = timeline
            .push(
                ItemKind::Tool,
                "Checked target · 5 lines\none\ntwo\nthree\nfour\nfive",
            )
            .expect("tool id");
        assert!(timeline.set_activity(tool, Some(ActivityState::Success)));

        let collapsed = timeline.rows(40);
        assert_eq!(collapsed.len(), 4);
        assert_eq!(
            collapsed
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            vec!["Checked target · 5 lines", "one", "two", "three"]
        );
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
            Some("Checked target · 5 lines\none\ntwo\nthree")
        );

        assert!(timeline.set_expanded(tool, true));
        assert_eq!(timeline.rows(40).len(), 6);
        assert_eq!(
            timeline.selected_text().as_deref(),
            Some("Checked target · 5 lines\none\ntwo\nthree\nfour\nfive")
        );
    }

    #[test]
    fn failed_tool_preview_keeps_two_head_rows_and_six_tail_rows() {
        let mut timeline = Timeline::new();
        let text = "Failed inspect · 10 lines\ncontext\nhidden two\nhidden three\nhidden four\ntail one\ntail two\ntail three\ntail four\ntail five\n終端 error 🧪";
        let tool = timeline.push(ItemKind::Tool, text).expect("tool id");
        let metadata_start = "Failed inspect".len();
        let metadata_end = text.find('\n').expect("headline boundary");
        assert!(timeline.set_tool_presentation(
            tool,
            ToolPresentation {
                source_lines: 10,
                source_bytes: text.len(),
                retained_lines: 10,
                retained_bytes: text.len(),
                duration_ms: Some(5),
                terminal_truncated: false,
                metadata_start,
                metadata_end,
            },
        ));
        assert!(timeline.set_activity(tool, Some(ActivityState::Error)));

        let collapsed = timeline.rows(80);
        assert_eq!(collapsed.len(), FAILED_TOOL_VISUAL_ROWS);
        assert_eq!(collapsed[0].text, "Failed inspect · 10 lines");
        assert_eq!(collapsed[1].text, "context");
        assert_eq!(collapsed[2].text, "tail one");
        assert_eq!(
            collapsed.last().map(|row| row.text.as_str()),
            Some("終端 error 🧪")
        );
        assert_eq!(collapsed[2].omitted_before_visual_rows, 3);
        assert!(collapsed
            .iter()
            .enumerate()
            .all(|(index, row)| index == 2 || row.omitted_before_visual_rows == 0));
        let disclosure = collapsed[0].disclosure.expect("failure disclosure");
        assert_eq!(disclosure.hidden_visual_rows, 3);
        assert_eq!(
            disclosure.tail_start_visual_row,
            Some(FAILED_TOOL_HEAD_ROWS)
        );

        let tail_start = text.find("tail one").expect("tail source byte");
        assert_eq!(
            Timeline::point_for_column(&collapsed[2], 0, false).byte,
            tail_start
        );
        timeline.start_selection(ContentPoint {
            item_id: tool,
            byte: 0,
        });
        timeline.extend_selection(ContentPoint {
            item_id: tool,
            byte: text.len(),
        });
        assert_eq!(
            timeline.selected_text().as_deref(),
            Some(
                "Failed inspect · 10 lines\ncontext\n… 3 earlier wrapped rows hidden …\ntail one\ntail two\ntail three\ntail four\ntail five\n終端 error 🧪"
            )
        );

        timeline.start_selection(ContentPoint {
            item_id: tool,
            byte: tail_start,
        });
        timeline.extend_selection(ContentPoint {
            item_id: tool,
            byte: text.len(),
        });
        assert_eq!(
            timeline.selected_text().as_deref(),
            Some("tail one\ntail two\ntail three\ntail four\ntail five\n終端 error 🧪")
        );

        assert!(timeline.set_expanded(tool, true));
        let expanded = timeline.rows(80);
        assert_eq!(expanded.len(), 11);
        assert!(expanded
            .iter()
            .all(|row| row.omitted_before_visual_rows == 0));
        timeline.start_selection(ContentPoint {
            item_id: tool,
            byte: 0,
        });
        timeline.extend_selection(ContentPoint {
            item_id: tool,
            byte: text.len(),
        });
        assert_eq!(timeline.selected_text().as_deref(), Some(text));
    }

    #[test]
    fn activity_changes_invalidate_the_collapsed_preview_budget() {
        let mut timeline = Timeline::new();
        let text = "inspect running\none\ntwo\nthree\nfour\nfive\nsix\nseven\neight";
        let tool = timeline.push(ItemKind::Tool, text).expect("tool");
        assert!(timeline.set_tool_presentation(
            tool,
            ToolPresentation {
                source_lines: 8,
                source_bytes: text.len(),
                retained_lines: 8,
                retained_bytes: text.len(),
                duration_ms: None,
                terminal_truncated: false,
                metadata_start: "inspect".len(),
                metadata_end: "inspect running".len(),
            },
        ));
        assert!(timeline.set_activity(tool, Some(ActivityState::Running)));
        assert_eq!(timeline.rows(40).len(), COLLAPSED_TOOL_VISUAL_ROWS);

        assert!(timeline.set_activity(tool, Some(ActivityState::Error)));
        let failed = timeline.rows(40);
        assert_eq!(failed.len(), FAILED_TOOL_VISUAL_ROWS);
        assert_eq!(failed[2].omitted_before_visual_rows, 1);
        assert!(failed.last().is_some_and(|row| row.text == "eight"));
        timeline.start_selection(ContentPoint {
            item_id: tool,
            byte: 0,
        });
        timeline.extend_selection(ContentPoint {
            item_id: tool,
            byte: text.len(),
        });
        assert!(timeline
            .selected_text()
            .is_some_and(|copy| copy.contains("… 1 earlier wrapped row hidden …")));

        assert!(timeline.set_activity(tool, Some(ActivityState::Cancelled)));
        let cancelled = timeline.rows(40);
        assert_eq!(cancelled.len(), COLLAPSED_TOOL_VISUAL_ROWS);
        assert!(cancelled
            .iter()
            .all(|row| row.omitted_before_visual_rows == 0));
    }

    #[test]
    fn tool_disclosure_distinguishes_hidden_rows_from_terminal_truncation() {
        let mut timeline = Timeline::new();
        let text = "Checked target · 7 lines · 5 ms\none\ntwo\nthree\nfour\nfive\nsix\nseven";
        let tool = timeline.push(ItemKind::Tool, text).expect("tool id");
        let metadata_start = "Checked target".len();
        let metadata_end = text.find('\n').expect("headline boundary");
        assert!(timeline.set_tool_presentation(
            tool,
            ToolPresentation {
                source_lines: 9,
                source_bytes: 4_096,
                retained_lines: 7,
                retained_bytes: text.len(),
                duration_ms: Some(5),
                terminal_truncated: true,
                metadata_start,
                metadata_end,
            },
        ));

        let collapsed = timeline.rows(80);
        let disclosure = collapsed[0].disclosure.expect("tool disclosure");
        assert!(!disclosure.expanded);
        assert!(disclosure.expandable);
        assert_eq!(disclosure.hidden_visual_rows, 4);
        assert!(disclosure.terminal_truncated);
        assert_eq!(collapsed[0].tool.expect("presentation").source_lines, 9);

        assert!(timeline.set_expanded(tool, true));
        let expanded = timeline.rows(80);
        let disclosure = expanded[0].disclosure.expect("expanded disclosure");
        assert!(disclosure.expanded);
        assert!(disclosure.expandable);
        assert_eq!(expanded.len(), 8);
        assert_eq!(timeline.items().len(), 1);
    }

    #[test]
    fn assistant_presentation_changes_without_moving_text_or_geometry() {
        let mut timeline = Timeline::new();
        let assistant = timeline
            .push(ItemKind::Assistant, "I will inspect the target")
            .expect("assistant");
        let tool = timeline
            .push(ItemKind::Tool, "inspect running")
            .expect("tool");
        timeline.start_selection(ContentPoint {
            item_id: assistant,
            byte: 0,
        });
        timeline.extend_selection(ContentPoint {
            item_id: assistant,
            byte: "I will inspect the target".len(),
        });
        assert!(timeline.hold_at(ContentPoint {
            item_id: assistant,
            byte: 0,
        }));

        let before = timeline.rows(16);
        let before_geometry = before
            .iter()
            .map(|row| {
                (
                    row.item_id,
                    row.start_byte,
                    row.end_byte,
                    row.text.clone(),
                    row.part,
                )
            })
            .collect::<Vec<_>>();
        let before_start = timeline.current_start(16);
        let before_copy = timeline.selected_text();
        assert!(before
            .iter()
            .filter(|row| row.item_id == assistant)
            .all(|row| { row.assistant_presentation == Some(AssistantPresentation::Answer) }));

        assert!(timeline.set_assistant_presentation(assistant, AssistantPresentation::Progress));
        assert!(!timeline.set_assistant_presentation(tool, AssistantPresentation::Progress));
        let after = timeline.rows(16);
        let after_geometry = after
            .iter()
            .map(|row| {
                (
                    row.item_id,
                    row.start_byte,
                    row.end_byte,
                    row.text.clone(),
                    row.part,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(after_geometry, before_geometry);
        assert_eq!(timeline.current_start(16), before_start);
        assert_eq!(timeline.selected_text(), before_copy);
        assert!(after
            .iter()
            .filter(|row| row.item_id == assistant)
            .all(|row| { row.assistant_presentation == Some(AssistantPresentation::Progress) }));
    }

    #[test]
    fn successful_tool_grouping_is_opt_in_thresholded_and_reversible() {
        let mut timeline = Timeline::new();
        let first = push_successful_tool(&mut timeline, "first", "detail one", Some(10));
        let second = push_successful_tool(&mut timeline, "second", "detail two", Some(20));
        let third = push_successful_tool(&mut timeline, "third", "detail three", Some(30));

        assert!(!timeline.group_successful_tools());
        assert_eq!(
            timeline
                .rows(80)
                .iter()
                .filter(|row| row.kind == ItemKind::Tool)
                .count(),
            6,
            "the compatibility default keeps all three original headlines and bodies"
        );

        timeline.set_group_successful_tools(true);
        let collapsed = timeline.rows(80);
        assert_eq!(collapsed.len(), 1);
        let summary = collapsed[0].tool_group.expect("group summary");
        assert_eq!(summary.tool_count, 3);
        assert_eq!(summary.duration_ms, Some(60));
        assert!(summary.has_retained_details);
        assert!(!summary.expanded);
        assert_eq!(timeline.items().len(), 3, "original items remain retained");

        assert!(timeline.move_tool_focus(true, 80, 12));
        assert_eq!(
            timeline.focused_target(),
            Some(TimelineFocusTarget::SuccessGroup(first))
        );
        let before = timeline.view(80, 12);
        let before_row = before
            .rows
            .iter()
            .position(|row| row.tool_group.is_some())
            .expect("collapsed group row");
        assert!(timeline.toggle_focused_tool(80, 12));
        let expanded = timeline.view(80, 12);
        let expanded_row = expanded
            .rows
            .iter()
            .position(|row| row.tool_group.is_some())
            .expect("expanded group row");
        assert_eq!(
            before_row, expanded_row,
            "the summary headline stays anchored"
        );
        assert!(expanded.rows[expanded_row]
            .tool_group
            .is_some_and(|group| group.expanded));
        assert!(expanded
            .rows
            .iter()
            .any(|row| row.item_id == first && row.tool_group.is_none()));
        assert!(expanded.rows.iter().any(|row| row.item_id == second));
        assert!(expanded.rows.iter().any(|row| row.item_id == third));

        assert!(timeline.move_tool_focus(true, 80, 12));
        assert_eq!(
            timeline.focused_target(),
            Some(TimelineFocusTarget::Tool(first))
        );
        assert!(timeline.focus_target(TimelineFocusTarget::SuccessGroup(first), 80, 12));
        assert!(timeline.toggle_focused_tool(80, 12));
        assert_eq!(timeline.rows(80).len(), 1);
    }

    #[test]
    fn grouping_boundaries_and_collapsed_copy_remain_truthful() {
        let mut timeline = Timeline::new();
        timeline.set_group_successful_tools(true);
        let before = timeline
            .push(ItemKind::Assistant, "before group")
            .expect("before");
        let first = push_successful_tool(&mut timeline, "one", "detail", Some(1));
        let second = push_successful_tool(&mut timeline, "two", "", Some(2));
        let third = push_successful_tool(&mut timeline, "three", "", None);
        let after = timeline
            .push(ItemKind::Assistant, "after group")
            .expect("after");
        let _ = push_successful_tool(&mut timeline, "only one", "", Some(4));
        let failed = timeline
            .push(ItemKind::Tool, "failed")
            .expect("failed boundary");
        assert!(timeline.set_activity(failed, Some(ActivityState::Error)));
        let _ = push_successful_tool(&mut timeline, "only two", "", Some(5));

        let rows = timeline.rows(80);
        let groups = rows
            .iter()
            .filter_map(|row| row.tool_group)
            .collect::<Vec<_>>();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].tool_count, 3);
        assert_eq!(groups[0].duration_ms, None, "unknown duration stays honest");
        assert!(rows.iter().any(|row| row.item_id == failed));

        timeline.start_selection(ContentPoint {
            item_id: before,
            byte: 0,
        });
        timeline.extend_selection(ContentPoint {
            item_id: after,
            byte: timeline.item(after).expect("after item").text.len(),
        });
        assert_eq!(
            timeline.selected_text().as_deref(),
            Some("before group\n… 3 successful tools grouped …\nafter group")
        );

        timeline.start_selection(ContentPoint {
            item_id: second,
            byte: 0,
        });
        timeline.extend_selection(ContentPoint {
            item_id: after,
            byte: timeline.item(after).expect("after item").text.len(),
        });
        assert_eq!(
            timeline.selected_text().as_deref(),
            Some("… 2 successful tools grouped …\nafter group"),
            "a selection retained before collapse never loses hidden members silently"
        );

        assert!(timeline.hold_at(ContentPoint {
            item_id: third,
            byte: 0,
        }));
        let group_row = timeline
            .rows(80)
            .iter()
            .position(|row| row.item_id == first && row.tool_group.is_some())
            .expect("group row");
        assert_eq!(timeline.current_start(80), group_row);
    }

    #[test]
    fn tool_focus_follows_reading_order_and_projects_on_the_same_rows() {
        let mut timeline = Timeline::new();
        let first = timeline
            .push(ItemKind::Tool, "first\none\ntwo\nthree\nfour")
            .expect("first tool");
        let _ = timeline.push(ItemKind::Assistant, "between");
        let second = timeline
            .push(ItemKind::Tool, "second\none\ntwo\nthree\nfour")
            .expect("second tool");

        assert!(timeline.move_tool_focus(true, 40, 8));
        assert_eq!(timeline.focused_tool(), Some(first));
        assert!(timeline
            .rows(40)
            .iter()
            .filter(|row| row.item_id == first)
            .all(|row| row.focused));

        assert!(timeline.move_tool_focus(true, 40, 8));
        assert_eq!(timeline.focused_tool(), Some(second));
        assert!(timeline.move_tool_focus(true, 40, 8));
        assert_eq!(timeline.focused_tool(), Some(second), "clamps at the end");
        assert!(timeline.move_tool_focus(false, 40, 8));
        assert_eq!(timeline.focused_tool(), Some(first));
        timeline.clear_tool_focus();
        assert!(timeline.rows(40).iter().all(|row| !row.focused));
    }

    #[test]
    fn focused_tool_headline_keeps_its_screen_row_across_expansion() {
        let mut timeline = Timeline::new();
        for number in 0..8 {
            let _ = timeline.push(ItemKind::Assistant, format!("before {number}"));
        }
        let tool = timeline
            .push(
                ItemKind::Tool,
                "tool completed\none\ntwo\nthree\nfour\nfive\nsix\nseven",
            )
            .expect("tool");
        for number in 0..12 {
            let _ = timeline.push(ItemKind::Assistant, format!("after {number}"));
        }
        let headline = timeline
            .rows(40)
            .iter()
            .position(|row| {
                row.item_id == tool
                    && matches!(row.part, VisualRowPart::Content { first: true, .. })
            })
            .expect("headline row");
        timeline.scroll_to_row(headline.saturating_sub(2), 40, 8);
        assert!(timeline.focus_tool(tool, 40, 8));
        let before = timeline.view(40, 8);
        let before_row = before
            .rows
            .iter()
            .position(|row| {
                row.item_id == tool
                    && matches!(row.part, VisualRowPart::Content { first: true, .. })
            })
            .expect("visible focused tool");

        assert!(timeline.toggle_focused_tool(40, 8));
        let expanded = timeline.view(40, 8);
        assert_eq!(
            expanded
                .rows
                .iter()
                .position(|row| {
                    row.item_id == tool
                        && matches!(row.part, VisualRowPart::Content { first: true, .. })
                })
                .expect("expanded headline"),
            before_row
        );
        assert!(timeline.toggle_focused_tool(40, 8));
        let collapsed = timeline.view(40, 8);
        assert_eq!(
            collapsed
                .rows
                .iter()
                .position(|row| {
                    row.item_id == tool
                        && matches!(row.part, VisualRowPart::Content { first: true, .. })
                })
                .expect("collapsed headline"),
            before_row
        );
    }

    #[test]
    fn collapsing_a_last_tool_uses_one_deterministic_bottom_clamp() {
        let mut timeline = Timeline::new();
        for number in 0..10 {
            let _ = timeline.push(ItemKind::Assistant, format!("before {number}"));
        }
        let tool = timeline
            .push(
                ItemKind::Tool,
                "tool completed\none\ntwo\nthree\nfour\nfive\nsix\nseven",
            )
            .expect("tool");
        assert!(timeline.set_expanded(tool, true));
        assert!(timeline.focus_tool(tool, 40, 6));
        let expanded = timeline.view(40, 6);
        assert_eq!(
            expanded
                .rows
                .iter()
                .position(|row| {
                    row.item_id == tool
                        && matches!(row.part, VisualRowPart::Content { first: true, .. })
                })
                .expect("expanded headline"),
            0
        );

        assert!(timeline.toggle_focused_tool(40, 6));
        let collapsed = timeline.view(40, 6);
        let headline_row = collapsed
            .rows
            .iter()
            .position(|row| {
                row.item_id == tool
                    && matches!(row.part, VisualRowPart::Content { first: true, .. })
            })
            .expect("clamped headline");
        assert_eq!(headline_row, 2);
        assert_eq!(
            collapsed.start,
            collapsed.total_rows - collapsed.viewport_rows
        );
        assert_eq!(timeline.viewport, ViewportAnchor::FollowBottom);
    }

    #[test]
    fn collapsed_tool_budget_is_cell_aware_at_narrow_widths_without_padding() {
        let mut timeline = Timeline::new();
        let short = timeline
            .push(ItemKind::Tool, "Read café\n済")
            .expect("short tool");
        assert_eq!(timeline.rows(40).len(), 2, "short output is never padded");

        let long = timeline
            .push(
                ItemKind::Tool,
                "Reading wide graphemes 😀😀😀😀\nαβγδεζηθ\ntail",
            )
            .expect("long tool");
        let rows = timeline.rows(8);
        let long_rows = rows
            .iter()
            .filter(|row| row.item_id == long)
            .collect::<Vec<_>>();
        assert_eq!(long_rows.len(), COLLAPSED_TOOL_VISUAL_ROWS);
        assert!(long_rows.iter().all(|row| {
            timeline
                .item(long)
                .expect("long tool")
                .text
                .is_char_boundary(row.start_byte)
                && timeline
                    .item(long)
                    .expect("long tool")
                    .text
                    .is_char_boundary(row.end_byte)
        }));
        assert_eq!(timeline.items().len(), 2);
        assert_eq!(
            timeline.item(short).expect("short tool").text,
            "Read café\n済"
        );
    }

    #[test]
    fn hidden_reasoning_leaves_geometry_index_stable_with_items_on_both_sides() {
        let mut t = Timeline::new();
        let user = t.push(ItemKind::User, "the question").expect("user");
        let reasoning = t
            .push(ItemKind::Reasoning, "thinking hard")
            .expect("reasoning");
        let assistant = t
            .push(ItemKind::Assistant, "the answer")
            .expect("assistant");

        // Visible: reasoning has rows and contributes to total height.
        assert!(t.rows(80).iter().any(|r| r.text.contains("thinking hard")));
        let total_visible = t.total_rows(80);

        t.set_reasoning_visible(false);

        // Hidden from render, but the items on both sides remain.
        let hidden_rows = t.rows(80);
        assert!(!hidden_rows.iter().any(|r| r.text.contains("thinking hard")));
        assert!(hidden_rows.iter().any(|r| r.text.contains("the question")));
        assert!(hidden_rows.iter().any(|r| r.text.contains("the answer")));
        assert!(
            t.total_rows(80) < total_visible,
            "hidden reasoning shrank the height"
        );

        // The layout keeps one entry per item (index identity), the hidden entry is
        // zero-height, and a row that used to belong to the assistant still resolves
        // to the assistant — not the hidden reasoning between them.
        let layout = t.layout_index(80);
        assert_eq!(layout.entries.len(), t.items().len());
        let r_index = t.item_positions[&reasoning];
        assert_eq!(
            layout.entries[r_index].start, layout.entries[r_index].end,
            "hidden reasoning is a zero-height entry"
        );
        let a_index = t.item_positions[&assistant];
        let a_start = layout.entries[a_index].start;
        let point = t.point_at_row(80, a_start).expect("point");
        assert_eq!(
            point.item_id, assistant,
            "the row maps past the hidden reasoning"
        );
        // Anchoring on the assistant still resolves to its (now-lower) row.
        let anchored = t.resolve_anchor(
            80,
            ContentPoint {
                item_id: assistant,
                byte: 0,
            },
        );
        assert_eq!(anchored, a_start);
        let _ = user;
    }

    #[test]
    fn held_anchor_inside_hidden_reasoning_collapses_to_its_boundary() {
        let mut t = Timeline::new();
        let _ = t.push(ItemKind::User, "the question").expect("user");
        let reasoning = t
            .push(ItemKind::Reasoning, "line one\nline two\nline three")
            .expect("reasoning");
        let assistant = t
            .push(ItemKind::Assistant, "the answer")
            .expect("assistant");

        // Hold at a non-zero byte deep inside the multi-line reasoning while visible.
        let mid = t
            .item(reasoning)
            .expect("reasoning")
            .text
            .find("line two")
            .expect("byte");
        assert!(t.hold_at(ContentPoint {
            item_id: reasoning,
            byte: mid,
        }));
        let anchor = ContentPoint {
            item_id: reasoning,
            byte: mid,
        };
        assert!(t.resolve_anchor(80, anchor) < t.total_rows(80));

        // Hide: the held anchor must collapse to the (zero-height) reasoning
        // boundary — never a wrapped-row offset into a later item or past the end.
        t.set_reasoning_visible(false);
        let total_hidden = t.total_rows(80);
        let layout = t.layout_index(80);
        let r_start = layout.entries[t.item_positions[&reasoning]].start;
        let resolved = t.resolve_anchor(80, anchor);
        assert_eq!(resolved, r_start.min(total_hidden.saturating_sub(1)));
        assert!(resolved < total_hidden, "no phantom scroll past the total");

        // The viewport start stays bounded and the assistant still maps correctly.
        let view = t.view(80, 6);
        assert!(view.start < total_hidden);
        let a_start = layout.entries[t.item_positions[&assistant]].start;
        assert_eq!(
            t.point_at_row(80, a_start).expect("point").item_id,
            assistant
        );
    }

    #[test]
    fn hidden_reasoning_is_retained_and_reappears_on_show() {
        let mut t = Timeline::new();
        let reasoning = t.push(ItemKind::Reasoning, "part one").expect("reasoning");
        t.set_reasoning_visible(false);
        // Streaming continues while hidden.
        assert!(t.append_text(reasoning, " part two"));
        assert!(!t.rows(80).iter().any(|r| r.text.contains("part")));
        // The raw item is retained (combined).
        assert_eq!(t.item(reasoning).expect("item").text, "part one part two");
        // Showing it again reveals the accumulated text.
        t.set_reasoning_visible(true);
        assert!(t
            .rows(80)
            .iter()
            .any(|r| r.text.contains("part one part two")));
    }

    #[test]
    fn selection_does_not_copy_hidden_reasoning() {
        let mut t = Timeline::new();
        let user = t.push(ItemKind::User, "alpha").expect("user");
        let _reasoning = t.push(ItemKind::Reasoning, "SECRET").expect("reasoning");
        let assistant = t.push(ItemKind::Assistant, "omega").expect("assistant");
        t.start_selection(ContentPoint {
            item_id: user,
            byte: 0,
        });
        t.extend_selection(ContentPoint {
            item_id: assistant,
            byte: t.item(assistant).expect("assistant").text.len(),
        });
        // Visible: the selection spans all three.
        assert_eq!(t.selected_text().as_deref(), Some("alpha\nSECRET\nomega"));
        // Hidden: the reasoning text is not spliced into the copy.
        t.set_reasoning_visible(false);
        assert_eq!(t.selected_text().as_deref(), Some("alpha\nomega"));
    }

    #[test]
    fn appending_hidden_reasoning_under_a_held_viewport_raises_no_new_content() {
        let mut t = Timeline::new();
        let prompt = t.push(ItemKind::User, "prompt").expect("user");
        // Hold the viewport away from the bottom.
        assert!(t.hold_at(ContentPoint {
            item_id: prompt,
            byte: 0,
        }));
        assert!(!matches!(t.viewport, ViewportAnchor::FollowBottom));
        t.set_reasoning_visible(false);
        let _ = t
            .push(ItemKind::Reasoning, "hidden stream")
            .expect("reasoning");
        assert!(
            !t.has_new_content(),
            "a hidden reasoning append must not raise new content"
        );
        // A visible append still does.
        let _ = t.push(ItemKind::Assistant, "visible").expect("assistant");
        assert!(t.has_new_content());
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
