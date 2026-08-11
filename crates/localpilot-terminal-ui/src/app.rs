use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;

use localpilot_slash::Mode;

use crate::editor::{EditorSnapshot, EditorToken, SubmittedInput};
use crate::presentation::semantic_ranges;
use crate::projection::{
    ActiveTool, ProjectionSet, SessionProjection, TimelineSearchState, WorkActivity,
};
use crate::{
    sanitize_text, ActivityState, ContentPoint, Editor, ItemId, ItemKind, PeerPane, ResultTone,
    SemanticRole, SessionHeader, StyledRange, TextStyle, Theme, Timeline, ToolPresentation,
};

const MAX_TOOL_DETAIL_BYTES: usize = 4 * 1024;
const MAX_TOOL_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_SETTINGS_QUERY_BYTES: usize = 256;
const MAX_REVIEWER_BYTES: usize = 128;
const MAX_LOCALMIND_VIEW_ROWS: usize = 1_000;
const DOUBLE_ESCAPE_WINDOW: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TabId {
    Session,
    LocalMind,
    Plan,
    Activity,
    Settings,
}

impl TabId {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Session => "Session",
            Self::LocalMind => "LocalMind",
            Self::Plan => "Plan",
            Self::Activity => "Activity",
            Self::Settings => "Settings",
        }
    }
}

/// The single authority for which full-frame body owns rendering and input.
/// Transient modal takeovers always sit above a persistent product tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActiveBody {
    Takeover,
    LocalMind,
    Session,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub version: String,
    pub provider: String,
    pub model: String,
    pub workspace: String,
    pub branch: Option<String>,
    pub workspace_dirty: Option<bool>,
    pub mode: Mode,
    pub profile: String,
    pub session_id: String,
    pub session_name: Option<String>,
}

#[derive(Debug, Clone)]
struct SharedHeader {
    version: String,
    workspace: String,
    branch: Option<String>,
    workspace_dirty: Option<bool>,
    mode: Mode,
    profile: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSupport {
    Color,
    NoColor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardSupport {
    Basic,
    Enhanced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCapabilities {
    pub color: ColorSupport,
    pub mouse_capture: bool,
    pub synchronized_output: bool,
    pub keyboard: KeyboardSupport,
    pub clipboard_write: bool,
    pub screen_reader: bool,
}

impl Default for TerminalCapabilities {
    fn default() -> Self {
        Self {
            color: ColorSupport::Color,
            mouse_capture: false,
            synchronized_output: false,
            keyboard: KeyboardSupport::Basic,
            clipboard_write: false,
            screen_reader: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Composer,
    Timeline,
}

/// The surface that owns input when an image attach is declined, so the host can
/// explain the refusal instead of dropping the paste silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageAttachBlock {
    NotComposer,
    Dialog,
    Takeover,
    ThemePicker,
    InputOverlay,
    ShellMode,
}

impl ImageAttachBlock {
    /// The user-facing notice explaining why an image paste was declined.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::NotComposer => "image paste works only in the message composer.",
            Self::Dialog => "close the open dialog before pasting an image.",
            Self::Takeover => "image paste isn't available in this view — press Esc first.",
            Self::ThemePicker => "close the theme picker before pasting an image.",
            Self::InputOverlay => "close the open input overlay before pasting an image.",
            Self::ShellMode => "images are not available in shell mode.",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkState {
    Idle,
    Busy { cancellation_requested: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InputAction {
    CancelOrExit,
    Escape,
    OpenReverseHistory,
    StashOrPop,
    CyclePeer,
    NavigateTimeline(TimelineNavigation),
    Insert(String),
    Paste(String),
    Backspace,
    Delete,
    MoveLeft,
    MoveRight,
    ForwardCharOrSearch,
    MoveWordLeft,
    MoveWordRight,
    MoveUp,
    MoveDown,
    MoveVisualStart,
    MoveVisualEnd,
    MoveLineStart,
    MoveLineEnd,
    MoveTextStart,
    MoveTextEnd,
    DeleteWordLeft,
    DeleteToLineStart,
    DeleteToLineEnd,
    OpenExternalEditor,
    AcceptCompletion,
    PreviousLocalMindSection,
    Submit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InputOverlay {
    ReverseHistory(ReverseHistoryState),
    Completion(CompletionState),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakeoverKind {
    Diff,
    Help,
    Sessions,
    Settings,
    /// A bounded, scrollable, copyable command report (`/tree`, `/skills`, …).
    Report,
    /// LocalMind's persistent product-tab body, rendered through the bounded
    /// full-screen surface machinery.
    LocalMind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalMindSection {
    Docs,
    Graph,
    Memory,
    Review,
    Skills,
    Audit,
}

impl LocalMindSection {
    pub const ALL: [Self; 6] = [
        Self::Docs,
        Self::Graph,
        Self::Memory,
        Self::Review,
        Self::Skills,
        Self::Audit,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Docs => "Docs",
            Self::Graph => "Graph",
            Self::Memory => "Memory",
            Self::Review => "Review",
            Self::Skills => "Skills",
            Self::Audit => "Audit",
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::Docs => Self::Graph,
            Self::Graph => Self::Memory,
            Self::Memory => Self::Review,
            Self::Review => Self::Skills,
            Self::Skills => Self::Audit,
            Self::Audit => Self::Docs,
        }
    }

    const fn previous(self) -> Self {
        match self {
            Self::Docs => Self::Audit,
            Self::Graph => Self::Docs,
            Self::Memory => Self::Graph,
            Self::Review => Self::Memory,
            Self::Skills => Self::Review,
            Self::Audit => Self::Skills,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct LocalMindReviewRow {
    pub id: String,
    pub state: String,
    pub session_id: String,
    pub summary: String,
    pub category: String,
    pub confidence: String,
    pub note: Option<String>,
    pub replacement: Option<String>,
    pub seen_count: i64,
    pub evidence: Option<String>,
    pub requires_edit: bool,
    pub promoted: bool,
}

impl fmt::Debug for LocalMindReviewRow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalMindReviewRow")
            .field("id", &self.id)
            .field("state", &self.state)
            .field("requires_edit", &self.requires_edit)
            .field("promoted", &self.promoted)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct LocalMindData {
    pub docs: Vec<String>,
    pub graph: Vec<String>,
    pub memory: Vec<String>,
    pub review: Vec<LocalMindReviewRow>,
    pub skills: Vec<String>,
    pub audit: Vec<String>,
}

impl LocalMindData {
    fn sanitize(self) -> Self {
        let lines = |items: Vec<String>| {
            let omitted = items.len().saturating_sub(MAX_LOCALMIND_VIEW_ROWS);
            let mut bounded: Vec<String> = items
                .into_iter()
                .take(MAX_LOCALMIND_VIEW_ROWS)
                .map(|item| sanitize_text(&item))
                .collect();
            if omitted > 0 {
                if bounded.len() == MAX_LOCALMIND_VIEW_ROWS {
                    bounded.pop();
                }
                bounded.push(format!("… {omitted} more rows omitted"));
            }
            bounded
        };
        Self {
            docs: lines(self.docs),
            graph: lines(self.graph),
            memory: lines(self.memory),
            review: self
                .review
                .into_iter()
                .take(MAX_LOCALMIND_VIEW_ROWS)
                .map(|row| LocalMindReviewRow {
                    id: sanitize_inline(&row.id),
                    state: sanitize_inline(&row.state),
                    session_id: sanitize_inline(&row.session_id),
                    summary: sanitize_text(&row.summary),
                    category: sanitize_inline(&row.category),
                    confidence: sanitize_inline(&row.confidence),
                    note: row.note.map(|value| sanitize_text(&value)),
                    replacement: row.replacement.map(|value| sanitize_text(&value)),
                    seen_count: row.seen_count,
                    evidence: row.evidence.map(|value| sanitize_text(&value)),
                    requires_edit: row.requires_edit,
                    promoted: row.promoted,
                })
                .collect(),
            skills: lines(self.skills),
            audit: lines(self.audit),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalMindReviewAction {
    Accept,
    Reject,
    Promote,
}

#[derive(Clone, PartialEq, Eq)]
pub struct LocalMindReviewIntent {
    pub candidate_id: String,
    pub reviewer: String,
    pub action: LocalMindReviewAction,
}

impl fmt::Debug for LocalMindReviewIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalMindReviewIntent")
            .field("candidate_id", &self.candidate_id)
            .field("reviewer", &"<redacted>")
            .field("action", &self.action)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
struct LocalMindState {
    section: LocalMindSection,
    data: LocalMindData,
    reviewer: String,
    editing_reviewer: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct LocalMindView<'a> {
    pub section: LocalMindSection,
    pub lines: &'a [String],
    pub review: &'a [LocalMindReviewRow],
    pub reviewer: &'a str,
    pub editing_reviewer: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiffPane {
    Files,
    Content,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Addition,
    Deletion,
    Hunk,
    Metadata,
}

#[derive(Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub kind: DiffLineKind,
    pub text: String,
}

impl fmt::Debug for DiffLine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiffLine")
            .field("old_line", &self.old_line)
            .field("new_line", &self.new_line)
            .field("kind", &self.kind)
            .field(
                "text",
                &format_args!("<{} bytes redacted>", self.text.len()),
            )
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct DiffFile {
    pub status: String,
    pub path: String,
    pub additions: usize,
    pub deletions: usize,
    pub lines: Vec<DiffLine>,
}

impl fmt::Debug for DiffFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiffFile")
            .field("status", &self.status)
            .field(
                "path",
                &format_args!("<{} bytes redacted>", self.path.len()),
            )
            .field("additions", &self.additions)
            .field("deletions", &self.deletions)
            .field("line_count", &self.lines.len())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
struct TakeoverState {
    kind: TakeoverKind,
    scroll: usize,
    file_scroll: usize,
    selected: usize,
    settings: Vec<SettingEntry>,
    settings_query: String,
    sessions: Vec<SessionEntry>,
    diff_files: Vec<DiffFile>,
    diff_pane: DiffPane,
    selected_file: usize,
    tree_visible: bool,
    report_title: String,
    report_lines: Vec<String>,
    localmind: Option<LocalMindState>,
}

impl fmt::Debug for TakeoverState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TakeoverState")
            .field("kind", &self.kind)
            .field("scroll", &self.scroll)
            .field("file_scroll", &self.file_scroll)
            .field("selected", &self.selected)
            .field("setting_count", &self.settings.len())
            .field(
                "settings_query",
                &format_args!("<{} bytes redacted>", self.settings_query.len()),
            )
            .field("session_count", &self.sessions.len())
            .field("diff_file_count", &self.diff_files.len())
            .field("diff_pane", &self.diff_pane)
            .field("selected_file", &self.selected_file)
            .field("tree_visible", &self.tree_visible)
            .field(
                "report",
                &format_args!(
                    "<{} lines, {} bytes redacted>",
                    self.report_lines.len(),
                    self.report_lines.iter().map(String::len).sum::<usize>()
                ),
            )
            .field(
                "localmind",
                &self.localmind.as_ref().map(|state| {
                    (
                        state.section,
                        state.data.docs.len()
                            + state.data.graph.len()
                            + state.data.memory.len()
                            + state.data.review.len()
                            + state.data.skills.len()
                            + state.data.audit.len(),
                    )
                }),
            )
            .finish()
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TakeoverView<'a> {
    pub kind: TakeoverKind,
    pub scroll: usize,
    pub file_scroll: usize,
    pub selected: usize,
    pub commands: &'a [CompletionCommand],
    pub settings: &'a [SettingEntry],
    pub settings_query: &'a str,
    pub sessions: &'a [SessionEntry],
    pub diff_files: &'a [DiffFile],
    pub diff_pane: DiffPane,
    pub selected_file: usize,
    pub tree_visible: bool,
    pub report_title: &'a str,
    pub report_lines: &'a [String],
    pub localmind: Option<LocalMindView<'a>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingEdit {
    CopyOnSelect,
    Theme,
}

#[derive(Clone, PartialEq, Eq)]
pub struct SettingEntry {
    pub section: String,
    pub name: String,
    pub value: String,
    pub description: String,
    pub edit: Option<SettingEdit>,
    pub is_default: bool,
}

impl fmt::Debug for SettingEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SettingEntry")
            .field("section", &self.section)
            .field("name", &self.name)
            .field(
                "value",
                &format_args!("<{} bytes redacted>", self.value.len()),
            )
            .field("edit", &self.edit)
            .field("is_default", &self.is_default)
            .finish()
    }
}

fn filtered_setting_indices(state: &TakeoverState) -> Vec<usize> {
    let query = state.settings_query.trim().to_lowercase();
    state
        .settings
        .iter()
        .enumerate()
        .filter_map(|(index, setting)| {
            (query.is_empty()
                || setting.name.to_lowercase().contains(&query)
                || setting.section.to_lowercase().contains(&query))
            .then_some(index)
        })
        .collect()
}

#[derive(Clone, PartialEq, Eq)]
pub struct SessionEntry {
    pub selector: String,
    pub name: Option<String>,
    pub message_count: usize,
    pub updated: Option<String>,
    pub current: bool,
}

impl fmt::Debug for SessionEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redacted_name = self
            .name
            .as_ref()
            .map(|name| format!("<{} bytes redacted>", name.len()));
        formatter
            .debug_struct("SessionEntry")
            .field(
                "selector",
                &format_args!("<{} bytes redacted>", self.selector.len()),
            )
            .field("name", &redacted_name)
            .field("message_count", &self.message_count)
            .field("updated", &self.updated)
            .field("current", &self.current)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SessionSelection {
    selector: String,
}

impl SessionSelection {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.selector
    }
}

impl fmt::Debug for SessionSelection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("SessionSelection")
            .field(&format_args!("<{} bytes redacted>", self.selector.len()))
            .finish()
    }
}

fn diff_line_is_navigable(line: &DiffLine) -> bool {
    matches!(
        line.kind,
        DiffLineKind::Context | DiffLineKind::Addition | DiffLineKind::Deletion
    )
}

fn first_diff_line(file: Option<&DiffFile>) -> usize {
    file.and_then(|file| file.lines.iter().position(diff_line_is_navigable))
        .unwrap_or(0)
}

fn move_diff_line(file: Option<&DiffFile>, current: usize, delta: isize) -> usize {
    let Some(file) = file else {
        return 0;
    };
    let first = first_diff_line(Some(file));
    let Some(last) = file.lines.iter().rposition(diff_line_is_navigable) else {
        return 0;
    };
    if delta > 0 {
        file.lines
            .iter()
            .enumerate()
            .skip(current.saturating_add(1))
            .filter_map(|(index, line)| diff_line_is_navigable(line).then_some(index))
            .nth(delta.unsigned_abs().saturating_sub(1))
            .unwrap_or(last)
    } else if delta < 0 {
        file.lines
            .iter()
            .enumerate()
            .take(current)
            .rev()
            .filter_map(|(index, line)| diff_line_is_navigable(line).then_some(index))
            .nth(delta.unsigned_abs().saturating_sub(1))
            .unwrap_or(first)
    } else if file.lines.get(current).is_some_and(diff_line_is_navigable) {
        current
    } else {
        first
    }
}

fn diff_line_at_or_after(file: Option<&DiffFile>, start: usize) -> usize {
    let Some(file) = file else {
        return 0;
    };
    file.lines
        .iter()
        .enumerate()
        .find_map(|(index, line)| (index >= start && diff_line_is_navigable(line)).then_some(index))
        .or_else(|| {
            file.lines
                .iter()
                .enumerate()
                .rev()
                .find_map(|(index, line)| diff_line_is_navigable(line).then_some(index))
        })
        .unwrap_or(0)
}

fn keep_selection_visible(scroll: &mut usize, selected: usize, total: usize, viewport: usize) {
    let viewport = viewport.max(1);
    if selected < *scroll {
        *scroll = selected;
    } else if selected >= scroll.saturating_add(viewport) {
        *scroll = selected.saturating_add(1).saturating_sub(viewport);
    }
    *scroll = (*scroll).min(total.saturating_sub(viewport));
}

pub(crate) fn sanitize_inline(text: &str) -> String {
    sanitize_text(text)
        .chars()
        .map(|character| match character {
            '\n' | '\t' => ' ',
            character => character,
        })
        .collect()
}

fn previous_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn previous_grapheme_boundary(text: &str, index: usize) -> usize {
    let index = index.min(text.len());
    if index == text.len() {
        return text.len();
    }
    text.grapheme_indices(true)
        .map(|(start, _)| start)
        .take_while(|start| *start <= index)
        .last()
        .unwrap_or(0)
}

fn next_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn bounded_inline_text(text: &str, maximum: usize) -> String {
    let text = sanitize_inline(text);
    if text.len() <= maximum {
        return text;
    }
    let suffix = "…";
    let end = previous_char_boundary(&text, maximum.saturating_sub(suffix.len()));
    format!("{}{suffix}", &text[..end])
}

fn bounded_view_text(text: &str, maximum: usize) -> String {
    let text = sanitize_text(text);
    if text.len() <= maximum {
        return text;
    }
    let marker = "\n… middle omitted from terminal view …\n";
    let available = maximum.saturating_sub(marker.len());
    let head_budget = available.saturating_mul(3) / 4;
    let head_end = previous_char_boundary(&text, head_budget);
    let tail_start = next_char_boundary(&text, text.len().saturating_sub(available - head_end));
    format!("{}{}{}", &text[..head_end], marker, &text[tail_start..])
}

fn format_tool_duration(duration_ms: u64) -> String {
    if duration_ms < 1_000 {
        format!("{duration_ms} ms")
    } else {
        format!("{:.1} s", duration_ms as f64 / 1_000.0)
    }
}

fn tool_output_body(output: &str) -> &str {
    output
        .split_once("\noutput:\n")
        .map_or(output, |(_, body)| body)
        .trim()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolHeadlineState {
    Running,
    Success,
    Error,
    Cancelled,
}

#[derive(Debug, Clone, Copy)]
struct ToolVerbs {
    running: &'static str,
    success: &'static str,
    error: &'static str,
    cancelled: &'static str,
}

fn builtin_tool_verbs(name: &str) -> Option<ToolVerbs> {
    let verbs = match name {
        "read_file" | "read_tool_output" => ToolVerbs {
            running: "Reading",
            success: "Read",
            error: "Read failed",
            cancelled: "Read cancelled",
        },
        "write_file" | "append_file" => ToolVerbs {
            running: "Writing",
            success: "Wrote",
            error: "Write failed",
            cancelled: "Write cancelled",
        },
        "edit_file" | "multi_edit" | "replace_in_file" | "apply_patch" => ToolVerbs {
            running: "Editing",
            success: "Edited",
            error: "Edit failed",
            cancelled: "Edit cancelled",
        },
        "list_files" => ToolVerbs {
            running: "Listing",
            success: "Listed",
            error: "List failed",
            cancelled: "List cancelled",
        },
        "find_files" => ToolVerbs {
            running: "Finding files",
            success: "Found files",
            error: "File search failed",
            cancelled: "File search cancelled",
        },
        "search_text" => ToolVerbs {
            running: "Searching",
            success: "Searched",
            error: "Search failed",
            cancelled: "Search cancelled",
        },
        "fetch" => ToolVerbs {
            running: "Fetching",
            success: "Fetched",
            error: "Fetch failed",
            cancelled: "Fetch cancelled",
        },
        "run_shell" => ToolVerbs {
            running: "Running",
            success: "Ran",
            error: "Command failed",
            cancelled: "Command cancelled",
        },
        _ => return None,
    };
    Some(verbs)
}

fn tool_action_headline(name: &str, detail: &str, state: ToolHeadlineState) -> String {
    let name = sanitize_inline(name);
    let name = if name.is_empty() { "tool" } else { &name };
    let detail = sanitize_inline(detail);
    let Some(verbs) = builtin_tool_verbs(name) else {
        let state = match state {
            ToolHeadlineState::Running => "running",
            ToolHeadlineState::Success => "completed",
            ToolHeadlineState::Error => "failed",
            ToolHeadlineState::Cancelled => "cancelled",
        };
        return if detail.is_empty() {
            format!("{name} {state}")
        } else {
            format!("{name} {state}: {detail}")
        };
    };
    let verb = match state {
        ToolHeadlineState::Running => verbs.running,
        ToolHeadlineState::Success => verbs.success,
        ToolHeadlineState::Error => verbs.error,
        ToolHeadlineState::Cancelled => verbs.cancelled,
    };
    if detail.is_empty() {
        verb.to_string()
    } else if matches!(
        state,
        ToolHeadlineState::Error | ToolHeadlineState::Cancelled
    ) {
        format!("{verb}: {detail}")
    } else {
        format!("{verb} {detail}")
    }
}

fn unified_diff_summary(body: &str) -> Option<(usize, usize)> {
    let mut old_header = false;
    let mut new_header = false;
    let mut in_hunk = false;
    let mut additions = 0usize;
    let mut deletions = 0usize;
    for line in body.lines() {
        if line.starts_with("diff --git ") {
            old_header = false;
            new_header = false;
            in_hunk = false;
        } else if line.starts_with("--- ") {
            old_header = true;
            new_header = false;
            in_hunk = false;
        } else if old_header && line.starts_with("+++ ") {
            new_header = true;
        } else if old_header && new_header && line.starts_with("@@") {
            in_hunk = true;
        } else if in_hunk && line.starts_with('+') {
            additions = additions.saturating_add(1);
        } else if in_hunk && line.starts_with('-') {
            deletions = deletions.saturating_add(1);
        }
    }
    (old_header && new_header && in_hunk).then_some((additions, deletions))
}

fn finished_tool_headline(
    name: &str,
    detail: &str,
    state: ToolHeadlineState,
    output_line_count: usize,
    diff_summary: Option<(usize, usize)>,
    duration_ms: u64,
    terminal_truncated: bool,
) -> (String, usize) {
    let line_unit = if output_line_count == 1 {
        "line"
    } else {
        "lines"
    };
    let mut headline = tool_action_headline(name, detail, state);
    let metadata_start = headline.len();
    if let Some((additions, deletions)) = diff_summary {
        headline.push_str(&format!(" · +{additions}/-{deletions}"));
    }
    headline.push_str(&format!(
        " · {output_line_count} {line_unit} · {}",
        format_tool_duration(duration_ms)
    ));
    if terminal_truncated {
        headline.push_str(" · terminal view truncated");
    }
    (headline, metadata_start)
}

fn tool_activity_styles(text: &str, headline_role: SemanticRole) -> Vec<StyledRange> {
    if text.is_empty() {
        return Vec::new();
    }
    let headline_end = text.find('\n').unwrap_or(text.len());
    let mut styles = vec![StyledRange {
        start_byte: 0,
        end_byte: headline_end,
        style: TextStyle::new(headline_role).bold(),
    }];
    if headline_end == text.len() {
        return styles;
    }

    let body_start = headline_end.saturating_add(1);
    styles.push(StyledRange {
        start_byte: headline_end,
        end_byte: body_start,
        style: TextStyle::new(SemanticRole::Muted),
    });
    let body = &text[body_start..];
    let is_unified_diff = unified_diff_summary(body).is_some();
    let mut cursor = body_start;
    for line in body.split_inclusive('\n') {
        let content = line.trim_end_matches('\n').trim_end_matches('\r');
        let role = if is_unified_diff && content.starts_with('+') && !content.starts_with("+++") {
            SemanticRole::Success
        } else if is_unified_diff && content.starts_with('-') && !content.starts_with("---") {
            SemanticRole::Error
        } else {
            SemanticRole::Muted
        };
        styles.push(StyledRange {
            start_byte: cursor,
            end_byte: cursor + line.len(),
            style: TextStyle::new(role),
        });
        cursor += line.len();
    }
    styles
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ThemePickerState {
    original: Theme,
    selected: usize,
    return_to: Option<Box<TakeoverState>>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ThemePickerView {
    pub original: Theme,
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompletionState {
    kind: CompletionKind,
    token: EditorToken,
    items: Vec<CompletionItem>,
    selected: usize,
    parent_command: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionKind {
    Command,
    CommandValue,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionCommand {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletionItem {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CompletionView<'a> {
    pub kind: CompletionKind,
    pub items: &'a [CompletionItem],
    pub selected: usize,
    pub loading: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReverseHistoryState {
    query: String,
    match_index: Option<usize>,
    original_draft: EditorSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReverseSearchView<'a> {
    pub query: &'a str,
    pub has_match: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TimelineSearchView<'a> {
    pub query: &'a str,
    pub current: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineNavigation {
    PageUp,
    PageDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakeoverNavigation {
    LineUp,
    LineDown,
    PageUp,
    PageDown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppCommand {
    None,
    CancelWork,
    Copy(String),
    Exit,
    NavigateTimeline(TimelineNavigation),
    NavigateTakeover(TakeoverNavigation),
    LocalMindReview(LocalMindReviewIntent),
    ActivateSession(SessionSelection),
    OpenExternalEditor,
    RunShell(UserShellCommand),
    RunSlash(SubmittedInput),
    Submit(SubmittedInput),
}

#[derive(Clone, PartialEq, Eq)]
pub struct UserShellCommand {
    command: String,
}

impl UserShellCommand {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.command
    }
}

impl fmt::Debug for UserShellCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UserShellCommand")
            .field(
                "command",
                &format_args!("<{} bytes redacted>", self.command.len()),
            )
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct UserShellOutput {
    success: bool,
    lines: Vec<String>,
}

impl UserShellOutput {
    #[must_use]
    pub fn captured(exit_code: i32, stdout: &str, stderr: &str) -> Self {
        Self {
            success: exit_code == 0,
            lines: stdout
                .lines()
                .chain(stderr.lines())
                .map(str::to_string)
                .collect(),
        }
    }

    #[must_use]
    pub fn diagnostic(is_error: bool, text: &str) -> Self {
        Self {
            success: !is_error,
            lines: text.lines().map(str::to_string).collect(),
        }
    }
}

impl fmt::Debug for UserShellOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UserShellOutput")
            .field("success", &self.success)
            .field("line_count", &self.lines.len())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryState {
    Healthy,
    Recovering,
    Degraded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopState {
    Done,
    Cancelled,
    Degraded,
    ProviderError,
    BudgetExceeded,
    NoProgress,
    TimedOut,
    Quiesced,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
}

impl UsageTotals {
    #[must_use]
    pub fn total(self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// The current shared candidate as live chrome: the revision and the full digest,
/// abbreviated only at render time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairStatusCandidate {
    pub revision: u64,
    pub full_digest: String,
}

/// Minimal live/terminal status for an exact-two collaboration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairStatus {
    pub completed_rounds: u32,
    pub max_rounds: u32,
    pub scheduled: Option<PeerPane>,
    pub candidate: Option<PairStatusCandidate>,
    pub agreements: [bool; 2],
    pub repairing: Option<PeerPane>,
    pub terminal: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    pub title: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogState {
    Trust(TrustDialog),
    Approval {
        tool: String,
        target: String,
        risk_class: String,
    },
    Question(QuestionDialog),
}

#[derive(Clone, PartialEq, Eq)]
pub struct TrustDialog {
    path: String,
    selected: usize,
    path_selection: Option<TrustPathSelection>,
}

impl fmt::Debug for TrustDialog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TrustDialog")
            .field(
                "path",
                &format_args!("<{} bytes redacted>", self.path.len()),
            )
            .field("selected", &self.selected)
            .field(
                "path_selection",
                &self.path_selection.as_ref().map(|selection| {
                    format_args!("<{} bytes redacted>", selection.source.len()).to_string()
                }),
            )
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
struct TrustPathSelection {
    source: String,
    anchor: usize,
    focus: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct TrustPathSelectionView<'a> {
    pub source: &'a str,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Copy)]
pub(crate) struct TrustView<'a> {
    pub path: &'a str,
    pub selected: usize,
    pub path_selection: Option<TrustPathSelectionView<'a>>,
}

impl fmt::Debug for TrustView<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TrustView")
            .field(
                "path",
                &format_args!("<{} bytes redacted>", self.path.len()),
            )
            .field("selected", &self.selected)
            .field(
                "path_selection",
                &self.path_selection.map(|selection| {
                    format_args!("<{} bytes redacted>", selection.source.len()).to_string()
                }),
            )
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustAction {
    None,
    ContinueSession,
    Remember,
    Deny,
}

#[derive(Clone, PartialEq, Eq)]
pub struct QuestionDialog {
    header: Option<String>,
    question: String,
    options: Vec<QuestionOption>,
    selected: usize,
    checked: Vec<bool>,
    multi_select: bool,
    editing_other: bool,
    other: String,
    other_cursor: usize,
    index: usize,
    total: usize,
}

impl fmt::Debug for QuestionDialog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QuestionDialog")
            .field(
                "question",
                &format_args!("<{} bytes redacted>", self.question.len()),
            )
            .field(
                "options",
                &format_args!("<{} redacted>", self.options.len()),
            )
            .field("selected", &self.selected)
            .field("checked", &self.checked)
            .field("multi_select", &self.multi_select)
            .field("editing_other", &self.editing_other)
            .field(
                "other",
                &format_args!("<{} bytes redacted>", self.other.len()),
            )
            .field("other_cursor", &self.other_cursor)
            .field("index", &self.index)
            .field("total", &self.total)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Copy)]
pub(crate) struct QuestionView<'a> {
    pub header: Option<&'a str>,
    pub question: &'a str,
    pub options: &'a [QuestionOption],
    pub selected: usize,
    pub checked: &'a [bool],
    pub multi_select: bool,
    pub editing_other: bool,
    pub other: &'a str,
    pub other_cursor: usize,
    pub index: usize,
    pub total: usize,
}

impl fmt::Debug for QuestionView<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QuestionView")
            .field(
                "question",
                &format_args!("<{} bytes redacted>", self.question.len()),
            )
            .field(
                "options",
                &format_args!("<{} redacted>", self.options.len()),
            )
            .field("selected", &self.selected)
            .field("checked", &self.checked)
            .field("multi_select", &self.multi_select)
            .field("editing_other", &self.editing_other)
            .field(
                "other",
                &format_args!("<{} bytes redacted>", self.other.len()),
            )
            .field("other_cursor", &self.other_cursor)
            .field("index", &self.index)
            .field("total", &self.total)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionResponse {
    Selected(Vec<String>),
    Other(String),
}

#[derive(Clone, PartialEq, Eq)]
pub enum QuestionAction {
    None,
    Submit(QuestionResponse),
    Cancel,
}

impl fmt::Debug for QuestionAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => formatter.write_str("None"),
            Self::Submit(QuestionResponse::Selected(answers)) => formatter
                .debug_tuple("Submit::Selected")
                .field(&format_args!("<{} answers redacted>", answers.len()))
                .finish(),
            Self::Submit(QuestionResponse::Other(answer)) => formatter
                .debug_tuple("Submit::Other")
                .field(&format_args!("<{} bytes redacted>", answer.len()))
                .finish(),
            Self::Cancel => formatter.write_str("Cancel"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeUpdate {
    Text(String),
    Reasoning(String),
    ToolStarted {
        id: String,
        name: String,
        detail: String,
    },
    ToolFinished {
        id: String,
        name: String,
        is_error: bool,
        cancelled: bool,
        output: String,
        duration_ms: u64,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
    },
    ContextUsage {
        used: usize,
        limit: usize,
    },
    Notice(String),
    Warning(String),
    Plan(Vec<PlanEntry>),
    QuotaPaused(String),
    Recovery(RecoveryState),
    ToolStuck {
        name: String,
        count: u32,
    },
    FilesTouched,
    SoftInterruptInjected,
    Stopped(StopState),
}

#[derive(Debug, Clone)]
pub struct AppModel {
    header: SharedHeader,
    projections: ProjectionSet,
    pair_status: Option<PairStatus>,
    pub capabilities: TerminalCapabilities,
    pub theme: Theme,
    pub tabs: Vec<TabId>,
    pub active_tab: TabId,
    pub editor: Editor,
    pub focus: Focus,
    pub exit_armed: bool,
    pub exit_requested: bool,
    escape_armed_at: Option<Instant>,
    print_transcript_on_exit: bool,
    copy_on_select: bool,
    default_copy_on_select: bool,
    default_theme: Theme,
    pub dialog: Option<DialogState>,
    dialog_peer: Option<PeerPane>,
    takeover: Option<TakeoverState>,
    localmind_tab: Option<TakeoverState>,
    theme_picker: Option<ThemePickerState>,
    quick_help: bool,
    /// Host-level canonical reasoning visibility (`/think`). Propagated to every
    /// projection timeline; reapplied when a timeline is reset.
    reasoning_visible: bool,
    input_overlay: Option<InputOverlay>,
    external_edit_snapshot: Option<EditorSnapshot>,
    stashed_draft: Option<EditorSnapshot>,
    command_catalog: Vec<CompletionCommand>,
    command_values: BTreeMap<String, Vec<CompletionItem>>,
    workspace_files: Vec<String>,
    workspace_files_ready: bool,
}

impl AppModel {
    #[must_use]
    pub fn new(header: Header, capabilities: TerminalCapabilities) -> Self {
        let (header, session) = Self::split_header(header);
        Self::with_projections(
            header,
            ProjectionSet::single(SessionProjection::new(session)),
            capabilities,
        )
    }

    /// Builds the backend-neutral state for two session projections in one
    /// shared terminal shell. The executable host remains responsible for
    /// driving both sessions and routing their events.
    #[must_use]
    pub fn new_pair(
        primary: Header,
        secondary: SessionHeader,
        capabilities: TerminalCapabilities,
    ) -> Self {
        let (header, primary) = Self::split_header(primary);
        let secondary = Self::sanitize_session_header(secondary);
        Self::with_projections(
            header,
            ProjectionSet::pair(
                SessionProjection::new(primary),
                SessionProjection::new(secondary),
            ),
            capabilities,
        )
    }

    fn split_header(header: Header) -> (SharedHeader, SessionHeader) {
        let Header {
            version,
            provider,
            model,
            workspace,
            branch,
            workspace_dirty,
            mode,
            profile,
            session_id,
            session_name,
        } = header;
        (
            SharedHeader {
                version: sanitize_text(&version),
                workspace: sanitize_text(&workspace),
                branch: branch.map(|branch| sanitize_text(&branch)),
                workspace_dirty,
                mode,
                profile: sanitize_text(&profile),
            },
            Self::sanitize_session_header(SessionHeader {
                provider,
                model,
                session_id,
                session_name,
            }),
        )
    }

    fn sanitize_session_header(header: SessionHeader) -> SessionHeader {
        SessionHeader {
            provider: sanitize_text(&header.provider),
            model: sanitize_text(&header.model),
            session_id: sanitize_inline(&header.session_id),
            session_name: header.session_name.map(|name| sanitize_inline(&name)),
        }
    }

    fn with_projections(
        header: SharedHeader,
        projections: ProjectionSet,
        capabilities: TerminalCapabilities,
    ) -> Self {
        let tabs = if projections.is_pair() {
            vec![TabId::Session]
        } else {
            vec![TabId::Session, TabId::LocalMind]
        };
        Self {
            header,
            projections,
            pair_status: None,
            capabilities,
            theme: Theme::Default,
            tabs,
            active_tab: TabId::Session,
            editor: Editor::default(),
            focus: Focus::Composer,
            exit_armed: false,
            exit_requested: false,
            escape_armed_at: None,
            print_transcript_on_exit: false,
            copy_on_select: false,
            default_copy_on_select: false,
            default_theme: Theme::Default,
            dialog: None,
            dialog_peer: None,
            takeover: None,
            localmind_tab: None,
            theme_picker: None,
            quick_help: false,
            reasoning_visible: true,
            input_overlay: None,
            external_edit_snapshot: None,
            stashed_draft: None,
            command_catalog: Vec::new(),
            command_values: BTreeMap::new(),
            workspace_files: Vec::new(),
            workspace_files_ready: false,
        }
    }

    #[must_use]
    pub const fn is_pair(&self) -> bool {
        self.projections.is_pair()
    }

    #[must_use]
    pub const fn active_pair_pane(&self) -> Option<PeerPane> {
        self.projections.active_pair_pane()
    }

    #[must_use]
    pub const fn pair_status(&self) -> Option<&PairStatus> {
        self.pair_status.as_ref()
    }

    /// Updates collaboration chrome without changing either session projection.
    /// Returns `false` for the ordinary single-session model.
    #[must_use]
    pub fn set_pair_status(&mut self, mut status: PairStatus) -> bool {
        if !self.is_pair() {
            return false;
        }
        status.terminal = status
            .terminal
            .map(|terminal| sanitize_inline(&terminal))
            .filter(|terminal| !terminal.is_empty());
        status.candidate = status.candidate.map(|candidate| PairStatusCandidate {
            revision: candidate.revision,
            full_digest: sanitize_inline(&candidate.full_digest),
        });
        self.pair_status = Some(status);
        true
    }

    /// Selects one peer as the target of keyboard input and the shared composer.
    /// Returns `false` for a single session, an already-active peer, or while a
    /// shared dialog owns focus.
    #[must_use]
    pub fn select_pair_pane(&mut self, peer: PeerPane) -> bool {
        if self.dialog.is_some() {
            return false;
        }
        self.projections.select(peer)
    }

    /// Selects the other peer without changing any shared or per-peer surface
    /// state. Returns `false` when peer switching is unavailable.
    #[must_use]
    pub fn cycle_pair_pane(&mut self) -> bool {
        let peer = match self.active_pair_pane() {
            Some(PeerPane::A) => PeerPane::B,
            Some(PeerPane::B) => PeerPane::A,
            None => return false,
        };
        self.select_pair_pane(peer)
    }

    pub(crate) const fn active_projection(&self) -> &SessionProjection {
        self.projections.active()
    }

    pub(crate) fn projection(&self, peer: PeerPane) -> Option<&SessionProjection> {
        self.projections.projection(peer)
    }

    #[must_use]
    pub fn shared_version(&self) -> &str {
        &self.header.version
    }

    #[must_use]
    pub fn shared_workspace(&self) -> &str {
        &self.header.workspace
    }

    #[must_use]
    pub fn shared_branch(&self) -> Option<&str> {
        self.header.branch.as_deref()
    }

    #[must_use]
    pub const fn shared_workspace_dirty(&self) -> Option<bool> {
        self.header.workspace_dirty
    }

    #[must_use]
    pub fn shared_mode(&self) -> &str {
        self.header.mode.label()
    }

    /// The typed operating-mode authority (Agent/Harness/Research). The submit
    /// guard and composer hint read this, never a mode string.
    #[must_use]
    pub const fn mode(&self) -> Mode {
        self.header.mode
    }

    /// The composer placeholder hint for the current mode, host-projected so the
    /// renderer never parses a mode string. Shown only when the editor is empty.
    #[must_use]
    pub const fn composer_hint(&self) -> Option<&'static str> {
        match self.header.mode {
            Mode::Research => Some("Research a topic — local + web per config"),
            Mode::Agent | Mode::Harness => None,
        }
    }

    #[must_use]
    pub fn shared_profile(&self) -> &str {
        &self.header.profile
    }

    #[must_use]
    pub fn active_provider(&self) -> &str {
        &self.projections.active().header.provider
    }

    #[must_use]
    pub fn active_model(&self) -> &str {
        &self.projections.active().header.model
    }

    #[must_use]
    pub fn active_session_id(&self) -> &str {
        &self.projections.active().header.session_id
    }

    #[must_use]
    pub fn active_session_name(&self) -> Option<&str> {
        self.projections.active().header.session_name.as_deref()
    }

    pub fn set_active_provider_model(&mut self, provider: String, model: String) {
        let header = &mut self.projections.active_mut().header;
        header.provider = provider;
        header.model = model;
    }

    pub fn set_active_session_id(&mut self, session_id: String) {
        self.projections.active_mut().header.session_id = session_id;
    }

    pub fn set_active_session_name(&mut self, session_name: Option<String>) {
        self.projections.active_mut().header.session_name = session_name;
    }

    /// Update the shared permission-profile label shown in the footer/settings.
    /// Sanitized consistently with header construction. The host must call this in
    /// the same branch that updates the permission engine, so the displayed
    /// profile never disagrees with the profile actually in force.
    pub fn set_shared_profile(&mut self, profile: &str) {
        self.header.profile = sanitize_text(profile);
    }

    /// Update the shared operating mode (Agent/Harness/Research) shown in the
    /// footer, settings, and composer hint. The host must call this in the same
    /// branch that updates the session's mode, so the displayed mode never
    /// disagrees with the mode actually in force.
    pub fn set_shared_mode(&mut self, mode: Mode) {
        self.header.mode = mode;
    }

    /// Whether reasoning items are currently shown in the timeline.
    #[must_use]
    pub const fn reasoning_visible(&self) -> bool {
        self.reasoning_visible
    }

    /// Toggle reasoning visibility (`/think`) across every projection timeline and
    /// return the new state. Raw reasoning items are retained (streaming continues
    /// while hidden); the layout, render, search, selection, and new-content
    /// surfaces all follow the flag, and any open search is refreshed so a hidden
    /// reasoning match cannot survive without a row.
    pub fn toggle_reasoning(&mut self) -> bool {
        self.reasoning_visible = !self.reasoning_visible;
        let visible = self.reasoning_visible;
        for projection in self.projections.iter_mut() {
            projection.timeline.set_reasoning_visible(visible);
            if projection.timeline_search.is_some() {
                Self::refresh_timeline_search_on(projection);
            }
        }
        visible
    }

    /// Reapply the canonical reasoning visibility to every projection timeline —
    /// called after a timeline reset (clear/new session) so hiding survives.
    fn reapply_reasoning_visibility(&mut self) {
        let visible = self.reasoning_visible;
        for projection in self.projections.iter_mut() {
            projection.timeline.set_reasoning_visible(visible);
        }
    }

    #[must_use]
    pub const fn active_timeline(&self) -> &Timeline {
        &self.projections.active().timeline
    }

    pub fn active_timeline_mut(&mut self) -> &mut Timeline {
        &mut self.projections.active_mut().timeline
    }

    /// Returns the named peer timeline, or `None` for an ordinary single
    /// session.
    #[must_use]
    pub fn timeline_for(&self, peer: PeerPane) -> Option<&Timeline> {
        self.projections
            .projection(peer)
            .map(|projection| &projection.timeline)
    }

    /// Returns the named peer timeline mutably, or `None` for an ordinary
    /// single session.
    pub fn timeline_for_mut(&mut self, peer: PeerPane) -> Option<&mut Timeline> {
        self.projections
            .projection_mut(peer)
            .map(|projection| &mut projection.timeline)
    }

    #[must_use]
    pub const fn active_work(&self) -> WorkState {
        self.projections.active().work
    }

    #[must_use]
    pub fn active_plan(&self) -> &[PlanEntry] {
        &self.projections.active().plan
    }

    #[must_use]
    pub const fn active_usage(&self) -> Option<UsageTotals> {
        self.projections.active().usage
    }

    pub fn set_active_usage(&mut self, usage: Option<UsageTotals>) {
        self.projections.active_mut().usage = usage;
    }

    #[must_use]
    pub const fn active_context_usage(&self) -> Option<(usize, usize)> {
        self.projections.active().context_usage
    }

    pub fn set_active_context_usage(&mut self, context_usage: Option<(usize, usize)>) {
        self.projections.active_mut().context_usage = context_usage;
    }

    #[must_use]
    pub const fn active_stream_bytes(&self) -> usize {
        self.projections.active().stream_bytes
    }

    /// Installs only tabs backed by a real LocalPilot surface, preserving the
    /// caller's order and ignoring duplicates. Session is the safe fallback.
    pub fn set_tabs(&mut self, tabs: impl IntoIterator<Item = TabId>) {
        let mut unique = Vec::new();
        for tab in tabs {
            if !unique.contains(&tab) {
                unique.push(tab);
            }
        }
        if unique.is_empty() {
            unique.push(TabId::Session);
        }
        if !unique.contains(&self.active_tab) {
            self.active_tab = unique[0];
        }
        self.tabs = unique;
    }

    /// Select a configured product tab without discarding the state owned by
    /// either tab. Hosts lazy-load LocalMind before the next frame when needed.
    pub fn activate_tab(&mut self, tab: TabId) -> bool {
        if !self.tabs.contains(&tab) {
            return false;
        }
        self.exit_armed = false;
        self.quick_help = false;
        self.input_overlay = None;
        self.active_tab = tab;
        true
    }

    /// Resolve body focus once so rendering and input cannot disagree about
    /// whether a transient modal, LocalMind, or the Session composer is active.
    #[must_use]
    pub(crate) const fn active_body(&self) -> ActiveBody {
        if self.takeover.is_some() {
            ActiveBody::Takeover
        } else if matches!(self.active_tab, TabId::LocalMind) && self.localmind_tab.is_some() {
            ActiveBody::LocalMind
        } else {
            ActiveBody::Session
        }
    }

    fn active_body_state_mut(&mut self) -> Option<&mut TakeoverState> {
        match self.active_body() {
            ActiveBody::Takeover => self.takeover.as_mut(),
            ActiveBody::LocalMind => self.localmind_tab.as_mut(),
            ActiveBody::Session => None,
        }
    }

    pub fn handle_input(&mut self, action: InputAction, editor_width: u16) -> AppCommand {
        self.handle_input_at(action, editor_width, Instant::now())
    }

    fn handle_input_at(
        &mut self,
        action: InputAction,
        editor_width: u16,
        now: Instant,
    ) -> AppCommand {
        if !matches!(action, InputAction::Escape) {
            self.escape_armed_at = None;
        }
        if matches!(action, InputAction::Escape)
            && (self.dialog.is_some()
                || self.theme_picker.is_some()
                || self.quick_help
                || self.takeover.is_some()
                || matches!(self.active_body(), ActiveBody::LocalMind)
                || self.has_input_overlay()
                || self.editor.is_shell_mode())
        {
            self.escape_armed_at = None;
        }
        if self.dialog.is_some() {
            if !matches!(action, InputAction::CancelOrExit) {
                self.exit_armed = false;
                return AppCommand::None;
            }
            return self.cancel_or_exit();
        }
        if matches!(action, InputAction::CyclePeer) {
            self.exit_armed = false;
            let _ = self.cycle_pair_pane();
            return AppCommand::None;
        }
        if matches!(action, InputAction::CancelOrExit) && self.theme_picker.is_some() {
            self.exit_armed = false;
            self.close_theme_picker(true);
            return AppCommand::None;
        }
        if matches!(action, InputAction::CancelOrExit)
            && self.quick_help
            && self.projections.active().timeline.selected_text().is_none()
        {
            self.exit_armed = false;
            self.quick_help = false;
            return AppCommand::None;
        }
        if matches!(action, InputAction::CancelOrExit) && self.takeover.is_some() {
            self.exit_armed = false;
            // Ctrl+C on a Report copies the whole bounded body (not the
            // breadcrumb); Esc still dismisses it via `handle_takeover_input`.
            if let Some(state) = &self.takeover {
                if state.kind == TakeoverKind::Report {
                    return AppCommand::Copy(state.report_lines.join("\n"));
                }
            }
            self.takeover = None;
            return AppCommand::None;
        }
        if matches!(action, InputAction::CancelOrExit)
            && matches!(self.active_body(), ActiveBody::LocalMind)
        {
            self.exit_armed = false;
            let lines = self
                .localmind_tab
                .as_ref()
                .and_then(|state| state.localmind.as_ref())
                .map(|localmind| match localmind.section {
                    LocalMindSection::Docs => &localmind.data.docs,
                    LocalMindSection::Graph => &localmind.data.graph,
                    LocalMindSection::Memory => &localmind.data.memory,
                    LocalMindSection::Review => &localmind.data.memory[..0],
                    LocalMindSection::Skills => &localmind.data.skills,
                    LocalMindSection::Audit => &localmind.data.audit,
                });
            return lines
                .filter(|lines| !lines.is_empty())
                .map_or(AppCommand::None, |lines| AppCommand::Copy(lines.join("\n")));
        }
        if matches!(action, InputAction::CancelOrExit) && self.has_input_overlay() {
            self.exit_armed = false;
            self.cancel_input_overlay();
            return AppCommand::None;
        }
        if !matches!(action, InputAction::CancelOrExit) {
            self.exit_armed = false;
        }
        if matches!(action, InputAction::CancelOrExit) {
            return self.cancel_or_exit();
        }
        if self.theme_picker.is_some() {
            return self.handle_theme_picker_input(action);
        }
        if self.takeover.is_some() {
            return self.handle_takeover_input(action);
        }
        if matches!(self.active_body(), ActiveBody::LocalMind) {
            return self.handle_localmind_input(action);
        }
        if self.has_input_overlay() {
            return self.handle_overlay_input(action);
        }
        if self.quick_help {
            match &action {
                InputAction::Escape => {
                    self.quick_help = false;
                    return AppCommand::None;
                }
                InputAction::Insert(text) if text == "?" && self.editor.text().is_empty() => {
                    self.quick_help = false;
                    return AppCommand::None;
                }
                InputAction::NavigateTimeline(_) => {}
                _ => self.quick_help = false,
            }
        }
        if matches!(action, InputAction::Escape) && self.editor.is_shell_mode() {
            self.escape_armed_at = None;
            self.editor.exit_shell_mode();
            return AppCommand::None;
        }
        match action {
            InputAction::CancelOrExit => AppCommand::None,
            InputAction::Escape => self.escape_or_interrupt(now),
            InputAction::OpenReverseHistory
                if self.focus == Focus::Composer && !self.editor.is_shell_mode() =>
            {
                self.open_reverse_history();
                AppCommand::None
            }
            InputAction::StashOrPop if self.focus == Focus::Composer => {
                self.stash_or_pop();
                AppCommand::None
            }
            InputAction::NavigateTimeline(navigation) => AppCommand::NavigateTimeline(navigation),
            InputAction::Insert(text) if self.focus == Focus::Composer => {
                if text == "?"
                    && self.editor.text().is_empty()
                    && !self.editor.is_shell_mode()
                    && self.projections.active().work == WorkState::Idle
                {
                    self.quick_help = true;
                    return AppCommand::None;
                }
                let text = if self.editor.text().is_empty()
                    && !self.editor.is_shell_mode()
                    && text.starts_with('!')
                {
                    self.editor.enter_shell_mode();
                    &text[1..]
                } else {
                    &text
                };
                if !text.is_empty() {
                    self.editor.insert(text);
                }
                self.refresh_or_open_completion();
                AppCommand::None
            }
            InputAction::Paste(text) if self.focus == Focus::Composer => {
                self.editor.insert_paste(text);
                AppCommand::None
            }
            InputAction::Backspace if self.focus == Focus::Composer => {
                self.editor.backspace();
                AppCommand::None
            }
            InputAction::Delete if self.focus == Focus::Composer => {
                self.editor.delete();
                AppCommand::None
            }
            InputAction::MoveLeft if self.focus == Focus::Composer => {
                self.editor.move_left();
                AppCommand::None
            }
            InputAction::MoveRight if self.focus == Focus::Composer => {
                self.editor.move_right();
                AppCommand::None
            }
            InputAction::ForwardCharOrSearch if self.focus == Focus::Composer => {
                if self.editor.text().is_empty() && !self.editor.is_shell_mode() {
                    self.open_timeline_search(String::new());
                } else {
                    self.editor.move_right();
                }
                AppCommand::None
            }
            InputAction::MoveWordLeft if self.focus == Focus::Composer => {
                self.editor.move_word_left();
                AppCommand::None
            }
            InputAction::MoveWordRight if self.focus == Focus::Composer => {
                self.editor.move_word_right();
                AppCommand::None
            }
            InputAction::MoveUp if self.focus == Focus::Composer => {
                self.editor.up_or_history(editor_width);
                AppCommand::None
            }
            InputAction::MoveDown if self.focus == Focus::Composer => {
                self.editor.down_or_history(editor_width);
                AppCommand::None
            }
            InputAction::MoveVisualStart if self.focus == Focus::Composer => {
                self.editor.move_visual_start(editor_width);
                AppCommand::None
            }
            InputAction::MoveVisualEnd if self.focus == Focus::Composer => {
                self.editor.move_visual_end(editor_width);
                AppCommand::None
            }
            InputAction::MoveLineStart if self.focus == Focus::Composer => {
                self.editor.move_line_start();
                AppCommand::None
            }
            InputAction::MoveLineEnd if self.focus == Focus::Composer => {
                self.editor.move_line_end();
                AppCommand::None
            }
            InputAction::MoveTextStart if self.focus == Focus::Composer => {
                self.editor.move_text_start();
                AppCommand::None
            }
            InputAction::MoveTextEnd if self.focus == Focus::Composer => {
                self.editor.move_text_end();
                AppCommand::None
            }
            InputAction::DeleteWordLeft if self.focus == Focus::Composer => {
                self.editor.delete_word_left();
                AppCommand::None
            }
            InputAction::DeleteToLineStart if self.focus == Focus::Composer => {
                self.editor.delete_to_line_start();
                AppCommand::None
            }
            InputAction::DeleteToLineEnd if self.focus == Focus::Composer => {
                self.editor.delete_to_line_end();
                AppCommand::None
            }
            InputAction::OpenExternalEditor
                if self.focus == Focus::Composer
                    && self.projections.active().work == WorkState::Idle =>
            {
                self.external_edit_snapshot = Some(self.editor.snapshot());
                AppCommand::OpenExternalEditor
            }
            InputAction::AcceptCompletion => AppCommand::None,
            InputAction::Submit if self.focus == Focus::Composer => {
                if self.editor.is_shell_mode() {
                    if self.editor.has_images() {
                        let _ = self.push_runtime_item(
                            ItemKind::Notice,
                            "remove image attachments before running a shell command",
                        );
                        AppCommand::None
                    } else {
                        self.editor
                            .submit_shell()
                            .map_or(AppCommand::None, |command| {
                                AppCommand::RunShell(UserShellCommand { command })
                            })
                    }
                } else if self.open_command_values_for_draft() {
                    AppCommand::None
                } else {
                    // `/help`, `/theme`, and `/search` are emitted as ordinary
                    // slash commands here and routed by the host, so their
                    // argument-bearing forms are handled truthfully rather than
                    // intercepted as bare tokens.
                    self.submit_editor()
                }
            }
            InputAction::Insert(_)
            | InputAction::Paste(_)
            | InputAction::OpenReverseHistory
            | InputAction::StashOrPop
            | InputAction::CyclePeer
            | InputAction::Backspace
            | InputAction::Delete
            | InputAction::MoveLeft
            | InputAction::MoveRight
            | InputAction::ForwardCharOrSearch
            | InputAction::MoveWordLeft
            | InputAction::MoveWordRight
            | InputAction::MoveUp
            | InputAction::MoveDown
            | InputAction::MoveVisualStart
            | InputAction::MoveVisualEnd
            | InputAction::MoveLineStart
            | InputAction::MoveLineEnd
            | InputAction::MoveTextStart
            | InputAction::MoveTextEnd
            | InputAction::DeleteWordLeft
            | InputAction::DeleteToLineStart
            | InputAction::DeleteToLineEnd
            | InputAction::OpenExternalEditor
            | InputAction::PreviousLocalMindSection
            | InputAction::Submit => AppCommand::None,
        }
    }

    pub fn apply_runtime(&mut self, update: RuntimeUpdate) {
        Self::apply_runtime_projection(self.projections.active_mut(), update);
    }

    /// Applies a runtime update to one named peer without changing the active
    /// pane. Returns `false` when this model does not contain a peer pair.
    #[must_use]
    pub fn apply_runtime_for(&mut self, peer: PeerPane, update: RuntimeUpdate) -> bool {
        let Some(projection) = self.projections.projection_mut(peer) else {
            return false;
        };
        Self::apply_runtime_projection(projection, update);
        true
    }

    fn apply_runtime_projection(projection: &mut SessionProjection, update: RuntimeUpdate) {
        match update {
            RuntimeUpdate::Text(text) => {
                projection.stream_bytes = projection.stream_bytes.saturating_add(text.len());
                if !Self::append_active_to(projection, projection.active_assistant, &text) {
                    projection.active_assistant =
                        Self::opening_segment_text(text).and_then(|text| {
                            Self::push_runtime_item_to(projection, ItemKind::Assistant, text)
                        });
                }
            }
            RuntimeUpdate::Reasoning(text) => {
                projection.stream_bytes = projection.stream_bytes.saturating_add(text.len());
                if !Self::append_active_to(projection, projection.active_reasoning, &text) {
                    projection.active_reasoning =
                        Self::opening_segment_text(text).and_then(|text| {
                            Self::push_runtime_item_to(projection, ItemKind::Reasoning, text)
                        });
                }
            }
            RuntimeUpdate::ToolStarted { id, name, detail } => {
                // A tool row is a boundary in the stream. Finalize the styling of
                // the open assistant/reasoning segments (each inter-tool segment
                // is complete once the tool starts), then retire them so text and
                // reasoning that arrive after the tool open their own items below
                // it, in stream order, instead of coalescing into the item that
                // preceded the tool.
                Self::style_transcript_on(projection);
                projection.active_assistant = None;
                projection.active_reasoning = None;
                let detail = bounded_inline_text(&detail, MAX_TOOL_DETAIL_BYTES);
                let question = name == "ask_user";
                let text = if question {
                    format!("Asking user {detail}")
                } else {
                    tool_action_headline(&name, &detail, ToolHeadlineState::Running)
                };
                let kind = if question {
                    ItemKind::Question
                } else {
                    ItemKind::Tool
                };
                if let Some(item) = Self::push_runtime_item_to(projection, kind, text) {
                    let _ = projection
                        .timeline
                        .set_activity(item, Some(ActivityState::Running));
                    Self::style_activity_on(projection, item, ActivityState::Running);
                    projection.active_tools.insert(
                        id,
                        ActiveTool {
                            item_id: item,
                            detail,
                        },
                    );
                }
            }
            RuntimeUpdate::ToolFinished {
                id,
                name,
                is_error,
                cancelled,
                output,
                duration_ms,
            } => {
                if name == "ask_user" {
                    let output =
                        bounded_inline_text(tool_output_body(&output), MAX_TOOL_DETAIL_BYTES);
                    if let Some(active) = projection.active_tools.remove(&id) {
                        let mut text = format!("Asked user {}", active.detail);
                        if !output.is_empty() {
                            text.push('\n');
                            text.push_str(&output);
                        }
                        let _ = projection.timeline.replace_text(active.item_id, text);
                        let activity = if cancelled {
                            ActivityState::Cancelled
                        } else if is_error {
                            ActivityState::Error
                        } else {
                            ActivityState::Success
                        };
                        let _ = projection
                            .timeline
                            .set_activity(active.item_id, Some(activity));
                        Self::style_activity_on(projection, active.item_id, activity);
                    } else {
                        let mut text = "Asked user".to_string();
                        if !output.is_empty() {
                            text.push('\n');
                            text.push_str(&output);
                        }
                        let _ = Self::push_runtime_item_to(projection, ItemKind::Question, text);
                    }
                    if projection.timeline_search.is_some() {
                        Self::refresh_timeline_search_on(projection);
                    }
                    return;
                }
                let headline_state = if cancelled {
                    ToolHeadlineState::Cancelled
                } else if is_error {
                    ToolHeadlineState::Error
                } else {
                    ToolHeadlineState::Success
                };
                let output_body = sanitize_text(tool_output_body(&output));
                let output_line_count = if output_body.is_empty() {
                    0
                } else {
                    output_body.lines().count()
                };
                let output_bytes = output_body.len();
                let diff_summary = unified_diff_summary(&output_body);
                let terminal_truncated = output_bytes > MAX_TOOL_OUTPUT_BYTES;
                let output = bounded_view_text(&output_body, MAX_TOOL_OUTPUT_BYTES);
                let retained_lines = if output.is_empty() {
                    0
                } else {
                    output.lines().count()
                };
                let retained_bytes = output.len();
                if let Some(active) = projection.active_tools.remove(&id) {
                    let (mut text, metadata_start) = finished_tool_headline(
                        &name,
                        &active.detail,
                        headline_state,
                        output_line_count,
                        diff_summary,
                        duration_ms,
                        terminal_truncated,
                    );
                    let metadata_end = text.len();
                    if !output.is_empty() {
                        text.push('\n');
                        text.push_str(&output);
                    }
                    let _ = projection.timeline.replace_text(active.item_id, text);
                    let _ = projection.timeline.set_tool_presentation(
                        active.item_id,
                        ToolPresentation {
                            source_lines: output_line_count,
                            source_bytes: output_bytes,
                            retained_lines,
                            retained_bytes,
                            terminal_truncated,
                            metadata_start,
                            metadata_end,
                        },
                    );
                    let activity = if cancelled {
                        ActivityState::Cancelled
                    } else if is_error {
                        ActivityState::Error
                    } else {
                        ActivityState::Success
                    };
                    let _ = projection
                        .timeline
                        .set_activity(active.item_id, Some(activity));
                    Self::style_activity_on(projection, active.item_id, activity);
                } else {
                    let (mut text, metadata_start) = finished_tool_headline(
                        &name,
                        "",
                        headline_state,
                        output_line_count,
                        diff_summary,
                        duration_ms,
                        terminal_truncated,
                    );
                    let metadata_end = text.len();
                    if !output.is_empty() {
                        text.push('\n');
                        text.push_str(&output);
                    }
                    if let Some(item) = Self::push_runtime_item_to(projection, ItemKind::Tool, text)
                    {
                        let _ = projection.timeline.set_tool_presentation(
                            item,
                            ToolPresentation {
                                source_lines: output_line_count,
                                source_bytes: output_bytes,
                                retained_lines,
                                retained_bytes,
                                terminal_truncated,
                                metadata_start,
                                metadata_end,
                            },
                        );
                        let activity = if cancelled {
                            ActivityState::Cancelled
                        } else if is_error {
                            ActivityState::Error
                        } else {
                            ActivityState::Success
                        };
                        let _ = projection.timeline.set_activity(item, Some(activity));
                        Self::style_activity_on(projection, item, activity);
                    }
                }
            }
            RuntimeUpdate::Usage {
                input_tokens,
                output_tokens,
                cached_input_tokens,
            } => {
                let mut usage = projection.usage.unwrap_or_default();
                usage.input_tokens = usage.input_tokens.saturating_add(input_tokens);
                usage.output_tokens = usage.output_tokens.saturating_add(output_tokens);
                usage.cached_input_tokens = usage
                    .cached_input_tokens
                    .saturating_add(cached_input_tokens);
                projection.usage = Some(usage);
            }
            RuntimeUpdate::ContextUsage { used, limit } => {
                projection.context_usage = Some((used, limit));
            }
            RuntimeUpdate::Notice(message)
            | RuntimeUpdate::Warning(message)
            | RuntimeUpdate::QuotaPaused(message) => {
                let _ = Self::push_runtime_item_to(projection, ItemKind::Notice, message);
            }
            RuntimeUpdate::Plan(plan) => {
                projection.plan = plan
                    .into_iter()
                    .map(|entry| PlanEntry {
                        title: sanitize_text(&entry.title),
                        status: sanitize_text(&entry.status),
                    })
                    .collect();
            }
            RuntimeUpdate::Recovery(state) => {
                if state != RecoveryState::Healthy {
                    let _ = Self::push_runtime_item_to(
                        projection,
                        ItemKind::Notice,
                        format!("recovery: {state:?}"),
                    );
                }
            }
            RuntimeUpdate::ToolStuck { name, count } => {
                let _ = Self::push_runtime_item_to(
                    projection,
                    ItemKind::Notice,
                    format!("tool {name} stopped after {count} repeated failures"),
                );
            }
            RuntimeUpdate::FilesTouched => {}
            RuntimeUpdate::SoftInterruptInjected => {
                Self::style_transcript_on(projection);
                projection.active_assistant = None;
                projection.active_reasoning = None;
                projection.active_tools.clear();
                projection.active_insert_before = None;
            }
            RuntimeUpdate::Stopped(_) => {
                Self::style_transcript_on(projection);
                projection.work = WorkState::Idle;
                projection.work_activity = None;
                projection.active_assistant = None;
                projection.active_reasoning = None;
                projection.active_tools.clear();
                projection.active_insert_before = None;
            }
        }
        if projection.timeline_search.is_some() {
            Self::refresh_timeline_search_on(projection);
        }
    }

    /// Normalize only a brand-new streamed assistant/reasoning segment. Provider
    /// framing newlines have no semantic content before the first prose row;
    /// once an item exists, subsequent deltas are appended byte-for-byte.
    fn opening_segment_text(text: String) -> Option<String> {
        let without_framing = text.trim_start_matches(['\r', '\n']);
        if without_framing.trim().is_empty() {
            None
        } else if without_framing.len() == text.len() {
            Some(text)
        } else {
            Some(without_framing.to_string())
        }
    }

    pub fn begin_work(&mut self) {
        self.begin_work_with_label("Working");
    }

    /// Mark the active projection busy and give its existing working chrome an
    /// honest high-level operation label. The monotonic start belongs to the
    /// projection so elapsed time resets exactly when a new operation begins.
    pub fn begin_work_with_label(&mut self, label: &str) {
        Self::begin_projection_work(self.projections.active_mut(), label, Instant::now());
    }

    /// Marks one collaboration peer busy without changing the selected pane.
    /// Returns `false` for the ordinary single-session model.
    #[must_use]
    pub fn begin_work_for(&mut self, peer: PeerPane) -> bool {
        if !self.is_pair() {
            return false;
        }
        let Some(projection) = self.projections.projection_mut(peer) else {
            return false;
        };
        Self::begin_projection_work(projection, "Working", Instant::now());
        true
    }

    fn begin_projection_work(projection: &mut SessionProjection, label: &str, started_at: Instant) {
        projection.work = WorkState::Busy {
            cancellation_requested: false,
        };
        projection.work_activity = Some(WorkActivity {
            label: sanitize_inline(label),
            started_at,
        });
        projection.active_assistant = None;
        projection.active_reasoning = None;
        projection.active_tools.clear();
        projection.stream_bytes = 0;
        projection.active_insert_before = None;
    }

    pub fn begin_work_before(&mut self, item: Option<ItemId>) {
        self.begin_work();
        self.projections.active_mut().active_insert_before = item;
    }

    /// The active operation's high-level label and monotonic elapsed time.
    /// `None` means the projection is idle; no session-age clock is exposed.
    #[must_use]
    pub fn active_work_activity(&self) -> Option<(&str, Duration)> {
        let activity = self.projections.active().work_activity.as_ref()?;
        Some((
            activity.label.as_str(),
            Instant::now().saturating_duration_since(activity.started_at),
        ))
    }

    pub fn clear_cancellation_request(&mut self) {
        if matches!(self.projections.active().work, WorkState::Busy { .. }) {
            self.projections.active_mut().work = WorkState::Busy {
                cancellation_requested: false,
            };
        }
    }

    /// Mark every busy collaboration peer as having its cancellation requested,
    /// without changing focus and without faking terminal completion. Idle or already
    /// terminal projections are left unchanged; the real terminal state still arrives
    /// later from the driver. Returns `false` and mutates nothing for the ordinary
    /// single-session model.
    #[must_use]
    pub fn request_pair_cancellation(&mut self) -> bool {
        if !self.is_pair() {
            return false;
        }
        for peer in [PeerPane::A, PeerPane::B] {
            if let Some(projection) = self.projections.projection_mut(peer) {
                if let WorkState::Busy {
                    cancellation_requested: false,
                } = projection.work
                {
                    projection.work = WorkState::Busy {
                        cancellation_requested: true,
                    };
                }
            }
        }
        true
    }

    #[must_use]
    pub(crate) fn reverse_search(&self) -> Option<ReverseSearchView<'_>> {
        let Some(InputOverlay::ReverseHistory(state)) = &self.input_overlay else {
            return None;
        };
        Some(ReverseSearchView {
            query: &state.query,
            has_match: state.match_index.is_some(),
        })
    }

    #[must_use]
    pub(crate) fn timeline_search(&self) -> Option<TimelineSearchView<'_>> {
        let state = self.projections.active().timeline_search.as_ref()?;
        Some(TimelineSearchView {
            query: &state.query,
            current: state
                .selected
                .map_or(0, |selected| selected.saturating_add(1)),
            total: state.matches.len(),
        })
    }

    #[must_use]
    pub const fn has_input_overlay(&self) -> bool {
        self.input_overlay.is_some() || self.projections.active().timeline_search.is_some()
    }

    #[must_use]
    pub const fn has_takeover(&self) -> bool {
        self.takeover.is_some()
    }

    #[must_use]
    pub const fn has_theme_picker(&self) -> bool {
        self.theme_picker.is_some()
    }

    #[must_use]
    pub(crate) const fn quick_help(&self) -> bool {
        self.quick_help
    }

    pub fn dismiss_quick_help(&mut self) -> bool {
        std::mem::take(&mut self.quick_help)
    }

    #[must_use]
    pub(crate) fn takeover(&self) -> Option<TakeoverView<'_>> {
        self.takeover
            .as_ref()
            .map(|state| self.takeover_view(state))
    }

    #[must_use]
    pub(crate) fn localmind_tab(&self) -> Option<TakeoverView<'_>> {
        matches!(self.active_body(), ActiveBody::LocalMind)
            .then(|| self.localmind_tab.as_ref())
            .flatten()
            .map(|state| self.takeover_view(state))
    }

    fn takeover_view<'a>(&'a self, state: &'a TakeoverState) -> TakeoverView<'a> {
        TakeoverView {
            kind: state.kind,
            scroll: state.scroll,
            file_scroll: state.file_scroll,
            selected: state.selected,
            commands: &self.command_catalog,
            settings: &state.settings,
            settings_query: &state.settings_query,
            sessions: &state.sessions,
            diff_files: &state.diff_files,
            diff_pane: state.diff_pane,
            selected_file: state.selected_file,
            tree_visible: state.tree_visible,
            report_title: &state.report_title,
            report_lines: &state.report_lines,
            localmind: state.localmind.as_ref().map(|localmind| {
                let lines: &[String] = match localmind.section {
                    LocalMindSection::Docs => &localmind.data.docs,
                    LocalMindSection::Graph => &localmind.data.graph,
                    LocalMindSection::Memory => &localmind.data.memory,
                    LocalMindSection::Review => &[],
                    LocalMindSection::Skills => &localmind.data.skills,
                    LocalMindSection::Audit => &localmind.data.audit,
                };
                LocalMindView {
                    section: localmind.section,
                    lines,
                    review: &localmind.data.review,
                    reviewer: &localmind.reviewer,
                    editing_reviewer: localmind.editing_reviewer,
                }
            }),
        }
    }

    #[must_use]
    pub(crate) fn theme_picker(&self) -> Option<ThemePickerView> {
        self.theme_picker.as_ref().map(|state| ThemePickerView {
            original: state.original,
            selected: state.selected,
        })
    }

    pub fn open_theme_picker(&mut self) {
        self.exit_armed = false;
        self.quick_help = false;
        self.input_overlay = None;
        self.takeover = None;
        let selected = Theme::ALL
            .iter()
            .position(|theme| *theme == self.theme)
            .unwrap_or(0);
        self.theme_picker = Some(ThemePickerState {
            original: self.theme,
            selected,
            return_to: None,
        });
    }

    fn open_theme_picker_from_settings(&mut self) {
        let Some(return_to) = self
            .takeover
            .take()
            .filter(|state| state.kind == TakeoverKind::Settings)
        else {
            return;
        };
        let selected = Theme::ALL
            .iter()
            .position(|theme| *theme == self.theme)
            .unwrap_or(0);
        self.theme_picker = Some(ThemePickerState {
            original: self.theme,
            selected,
            return_to: Some(Box::new(return_to)),
        });
    }

    pub fn select_theme(&mut self, selected: usize) {
        let Some(theme) = Theme::ALL.get(selected).copied() else {
            return;
        };
        let Some(picker) = self.theme_picker.as_mut() else {
            return;
        };
        picker.selected = selected;
        self.theme = theme;
    }

    /// Apply a theme by value (e.g. from `/theme dim`), preserving the settings
    /// invariants a raw field write would skip. Any open theme picker is
    /// closed/restored FIRST — closing it after assignment would restore the
    /// picker's original theme and discard the requested one.
    pub fn apply_theme(&mut self, theme: Theme) {
        if self.theme_picker.is_some() {
            self.close_theme_picker(true);
        }
        self.theme = theme;
        self.sync_setting_values();
    }

    pub fn open_help(&mut self) {
        self.exit_armed = false;
        self.quick_help = false;
        self.close_theme_picker(true);
        self.input_overlay = None;
        self.takeover = Some(TakeoverState {
            kind: TakeoverKind::Help,
            scroll: 0,
            file_scroll: 0,
            selected: 0,
            settings: Vec::new(),
            settings_query: String::new(),
            sessions: Vec::new(),
            diff_files: Vec::new(),
            diff_pane: DiffPane::Content,
            selected_file: 0,
            tree_visible: true,
            report_title: String::new(),
            report_lines: Vec::new(),
            localmind: None,
        });
    }

    /// Open a bounded, scrollable, copyable command-report takeover. The title
    /// and body are defensively sanitized (the presenter is the bounding
    /// authority for length/bytes); Ctrl+C copies the body, Esc dismisses.
    pub fn open_report(&mut self, title: String, lines: Vec<String>) {
        self.exit_armed = false;
        self.quick_help = false;
        self.close_theme_picker(true);
        self.input_overlay = None;
        let title = {
            // Inline sanitize flattens newlines/tabs so the title bar can never
            // become multi-line; then clamp to the byte budget on a char boundary.
            let sanitized = sanitize_inline(&title);
            let mut end = sanitized.len().min(256);
            while end > 0 && !sanitized.is_char_boundary(end) {
                end -= 1;
            }
            sanitized[..end].to_string()
        };
        let report_lines: Vec<String> = lines.iter().map(|line| sanitize_text(line)).collect();
        self.takeover = Some(TakeoverState {
            kind: TakeoverKind::Report,
            scroll: 0,
            file_scroll: 0,
            selected: 0,
            settings: Vec::new(),
            settings_query: String::new(),
            sessions: Vec::new(),
            diff_files: Vec::new(),
            diff_pane: DiffPane::Content,
            selected_file: 0,
            tree_visible: true,
            report_title: title,
            report_lines,
            localmind: None,
        });
    }

    /// Open LocalMind's six internal sections as a persistent product tab. The
    /// host has already bounded and mapped the engine data; this layer only
    /// sanitizes it.
    pub fn open_localmind(&mut self, data: LocalMindData) {
        if !self.tabs.contains(&TabId::LocalMind) {
            return;
        }
        self.exit_armed = false;
        self.quick_help = false;
        self.close_theme_picker(true);
        self.input_overlay = None;
        self.takeover = None;
        self.active_tab = TabId::LocalMind;
        self.localmind_tab = Some(TakeoverState {
            kind: TakeoverKind::LocalMind,
            scroll: 0,
            file_scroll: 0,
            selected: 0,
            settings: Vec::new(),
            settings_query: String::new(),
            sessions: Vec::new(),
            diff_files: Vec::new(),
            diff_pane: DiffPane::Content,
            selected_file: 0,
            tree_visible: true,
            report_title: String::new(),
            report_lines: Vec::new(),
            localmind: Some(LocalMindState {
                section: LocalMindSection::Docs,
                data: data.sanitize(),
                reviewer: String::new(),
                editing_reviewer: false,
            }),
        });
    }

    /// Refresh LocalMind data after a review mutation while preserving the
    /// user's section, reviewer identity, and nearest valid selection.
    pub fn refresh_localmind(&mut self, data: LocalMindData) {
        let Some(state) = self
            .localmind_tab
            .as_mut()
            .filter(|state| state.kind == TakeoverKind::LocalMind)
        else {
            self.open_localmind(data);
            return;
        };
        let Some(localmind) = state.localmind.as_mut() else {
            self.open_localmind(data);
            return;
        };
        localmind.data = data.sanitize();
        state.selected = state
            .selected
            .min(localmind.data.review.len().saturating_sub(1));
        state.scroll = state.scroll.min(state.selected);
    }

    #[must_use]
    pub fn localmind_section(&self) -> Option<LocalMindSection> {
        self.localmind_tab
            .as_ref()
            .and_then(|state| state.localmind.as_ref())
            .map(|state| state.section)
    }

    #[cfg(test)]
    #[must_use]
    fn localmind_reviewer(&self) -> Option<&str> {
        self.localmind_tab
            .as_ref()
            .and_then(|state| state.localmind.as_ref())
            .map(|state| state.reviewer.as_str())
    }

    fn cycle_localmind_section(&mut self, forward: bool) {
        let Some(localmind) = self
            .localmind_tab
            .as_mut()
            .and_then(|state| state.localmind.as_mut())
        else {
            return;
        };
        localmind.section = if forward {
            localmind.section.next()
        } else {
            localmind.section.previous()
        };
        localmind.editing_reviewer = false;
        if let Some(state) = self.localmind_tab.as_mut() {
            state.scroll = 0;
            state.selected = 0;
        }
    }

    fn append_localmind_reviewer(&mut self, text: &str) {
        let Some(localmind) = self
            .localmind_tab
            .as_mut()
            .and_then(|state| state.localmind.as_mut())
            .filter(|state| state.editing_reviewer)
        else {
            return;
        };
        let text = sanitize_inline(text);
        let remaining = MAX_REVIEWER_BYTES.saturating_sub(localmind.reviewer.len());
        let end = previous_grapheme_boundary(&text, remaining);
        localmind.reviewer.push_str(&text[..end]);
    }

    fn backspace_localmind_reviewer(&mut self) {
        let Some(localmind) = self
            .localmind_tab
            .as_mut()
            .and_then(|state| state.localmind.as_mut())
            .filter(|state| state.editing_reviewer)
        else {
            return;
        };
        if let Some((start, _)) = localmind.reviewer.grapheme_indices(true).next_back() {
            localmind.reviewer.truncate(start);
        }
    }

    fn localmind_review_is_active(&self) -> bool {
        matches!(self.active_body(), ActiveBody::LocalMind)
            && self
                .localmind_tab
                .as_ref()
                .filter(|state| state.kind == TakeoverKind::LocalMind)
                .and_then(|state| state.localmind.as_ref())
                .is_some_and(|localmind| localmind.section == LocalMindSection::Review)
    }

    fn localmind_review_intent(
        &mut self,
        action: LocalMindReviewAction,
    ) -> Option<LocalMindReviewIntent> {
        let state = self
            .localmind_tab
            .as_mut()
            .filter(|state| state.kind == TakeoverKind::LocalMind)?;
        let localmind = state.localmind.as_mut()?;
        if localmind.section != LocalMindSection::Review {
            return None;
        }
        if localmind.reviewer.trim().is_empty() {
            localmind.editing_reviewer = true;
            return None;
        }
        let row = localmind.data.review.get(state.selected)?;
        let allowed = match action {
            LocalMindReviewAction::Accept => {
                row.state.eq_ignore_ascii_case("pending") && !row.requires_edit && !row.promoted
            }
            LocalMindReviewAction::Reject => row.state.eq_ignore_ascii_case("pending"),
            LocalMindReviewAction::Promote => match row.state.to_ascii_lowercase().as_str() {
                "accepted" => !row.requires_edit && !row.promoted,
                "edited" => !row.promoted,
                _ => false,
            },
        };
        allowed.then(|| LocalMindReviewIntent {
            candidate_id: row.id.clone(),
            reviewer: localmind.reviewer.trim().to_string(),
            action,
        })
    }

    pub fn open_settings(&mut self, settings: impl IntoIterator<Item = SettingEntry>) {
        self.exit_armed = false;
        self.quick_help = false;
        self.close_theme_picker(true);
        self.input_overlay = None;
        self.takeover = Some(TakeoverState {
            kind: TakeoverKind::Settings,
            scroll: 0,
            file_scroll: 0,
            selected: 0,
            settings: settings
                .into_iter()
                .map(|entry| SettingEntry {
                    section: sanitize_inline(&entry.section),
                    name: sanitize_inline(&entry.name),
                    value: sanitize_inline(&entry.value),
                    description: sanitize_inline(&entry.description),
                    edit: entry.edit,
                    is_default: entry.is_default,
                })
                .collect(),
            settings_query: String::new(),
            sessions: Vec::new(),
            diff_files: Vec::new(),
            diff_pane: DiffPane::Content,
            selected_file: 0,
            tree_visible: true,
            report_title: String::new(),
            report_lines: Vec::new(),
            localmind: None,
        });
    }

    /// Open settings with the filter pre-filled from `query` (e.g. from
    /// `/settings mouse`). The query goes through the same sanitize, byte-cap,
    /// and selection-reset path as interactive typing.
    pub fn open_settings_with_query(
        &mut self,
        settings: impl IntoIterator<Item = SettingEntry>,
        query: &str,
    ) {
        self.open_settings(settings);
        self.append_settings_query(query);
    }

    fn append_settings_query(&mut self, text: &str) {
        let Some(state) = self
            .takeover
            .as_mut()
            .filter(|state| state.kind == TakeoverKind::Settings)
        else {
            return;
        };
        let text = sanitize_inline(text);
        let remaining = MAX_SETTINGS_QUERY_BYTES.saturating_sub(state.settings_query.len());
        let end = previous_char_boundary(&text, remaining);
        state.settings_query.push_str(&text[..end]);
        state.selected = 0;
        state.scroll = 0;
    }

    fn backspace_settings_query(&mut self) {
        let Some(state) = self
            .takeover
            .as_mut()
            .filter(|state| state.kind == TakeoverKind::Settings)
        else {
            return;
        };
        if let Some((start, _)) = state.settings_query.grapheme_indices(true).next_back() {
            state.settings_query.truncate(start);
        }
        state.selected = 0;
        state.scroll = 0;
    }

    fn clear_settings_query(&mut self) -> bool {
        let Some(state) = self
            .takeover
            .as_mut()
            .filter(|state| state.kind == TakeoverKind::Settings)
        else {
            return false;
        };
        if state.settings_query.is_empty() {
            return false;
        }
        state.settings_query.clear();
        state.selected = 0;
        state.scroll = 0;
        true
    }

    fn edit_selected_setting(&mut self, reset: bool) {
        let edit = self.takeover.as_ref().and_then(|state| {
            (state.kind == TakeoverKind::Settings)
                .then(|| filtered_setting_indices(state))
                .and_then(|indices| indices.get(state.selected).copied())
                .and_then(|index| state.settings.get(index))
                .and_then(|setting| setting.edit)
        });
        match edit {
            Some(SettingEdit::CopyOnSelect) => {
                self.copy_on_select = if reset {
                    self.default_copy_on_select
                } else {
                    !self.copy_on_select
                };
                self.sync_setting_values();
            }
            Some(SettingEdit::Theme) if reset => {
                self.theme = self.default_theme;
                self.sync_setting_values();
            }
            Some(SettingEdit::Theme) => self.open_theme_picker_from_settings(),
            None => {}
        }
    }

    pub fn open_diff(&mut self, files: impl IntoIterator<Item = DiffFile>) {
        self.exit_armed = false;
        self.quick_help = false;
        self.close_theme_picker(true);
        self.input_overlay = None;
        let diff_files = files
            .into_iter()
            .map(|file| DiffFile {
                status: sanitize_inline(&file.status),
                path: sanitize_inline(&file.path),
                additions: file.additions,
                deletions: file.deletions,
                lines: file
                    .lines
                    .into_iter()
                    .map(|line| DiffLine {
                        old_line: line.old_line,
                        new_line: line.new_line,
                        kind: line.kind,
                        text: sanitize_text(&line.text),
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        let selected = first_diff_line(diff_files.first());
        self.takeover = Some(TakeoverState {
            kind: TakeoverKind::Diff,
            scroll: 0,
            file_scroll: 0,
            selected,
            settings: Vec::new(),
            settings_query: String::new(),
            sessions: Vec::new(),
            diff_files,
            diff_pane: DiffPane::Content,
            selected_file: 0,
            tree_visible: true,
            report_title: String::new(),
            report_lines: Vec::new(),
            localmind: None,
        });
    }

    pub fn open_sessions(&mut self, sessions: impl IntoIterator<Item = SessionEntry>) {
        self.exit_armed = false;
        self.quick_help = false;
        self.close_theme_picker(true);
        self.input_overlay = None;
        let sessions = sessions
            .into_iter()
            .map(|entry| SessionEntry {
                selector: sanitize_inline(&entry.selector),
                name: entry.name.map(|name| sanitize_inline(&name)),
                message_count: entry.message_count,
                updated: entry.updated.map(|updated| sanitize_inline(&updated)),
                current: entry.current,
            })
            .collect::<Vec<_>>();
        let selected = sessions.iter().position(|entry| entry.current).unwrap_or(0);
        self.takeover = Some(TakeoverState {
            kind: TakeoverKind::Sessions,
            scroll: selected,
            file_scroll: 0,
            selected,
            settings: Vec::new(),
            settings_query: String::new(),
            sessions,
            diff_files: Vec::new(),
            diff_pane: DiffPane::Content,
            selected_file: 0,
            tree_visible: true,
            report_title: String::new(),
            report_lines: Vec::new(),
            localmind: None,
        });
    }

    pub fn select_takeover_row(&mut self, selected: usize) {
        let Some(state) = self.active_body_state_mut() else {
            return;
        };
        match state.kind {
            TakeoverKind::Settings if selected < filtered_setting_indices(state).len() => {
                state.selected = selected;
            }
            TakeoverKind::Sessions if selected < state.sessions.len() => state.selected = selected,
            TakeoverKind::LocalMind
                if state.localmind.as_ref().is_some_and(|localmind| {
                    localmind.section == LocalMindSection::Review
                        && selected < localmind.data.review.len()
                }) =>
            {
                state.selected = selected;
            }
            TakeoverKind::Diff
                if state
                    .diff_files
                    .get(state.selected_file)
                    .and_then(|file| file.lines.get(selected))
                    .is_some_and(diff_line_is_navigable) =>
            {
                state.selected = selected;
                state.diff_pane = DiffPane::Content;
            }
            TakeoverKind::Diff
            | TakeoverKind::Help
            | TakeoverKind::Sessions
            | TakeoverKind::Settings
            | TakeoverKind::Report
            | TakeoverKind::LocalMind => {}
        }
    }

    pub fn select_diff_file(&mut self, selected: usize) {
        let Some(state) = self.active_body_state_mut() else {
            return;
        };
        if state.kind == TakeoverKind::Diff && selected < state.diff_files.len() {
            state.selected_file = selected;
            state.selected = first_diff_line(state.diff_files.get(selected));
            state.scroll = 0;
            state.diff_pane = DiffPane::Files;
        }
    }

    pub fn scroll_takeover_by(&mut self, delta: isize, total_rows: usize, viewport_rows: usize) {
        let Some(state) = self.active_body_state_mut() else {
            return;
        };
        let localmind_review = state.kind == TakeoverKind::LocalMind
            && state
                .localmind
                .as_ref()
                .is_some_and(|localmind| localmind.section == LocalMindSection::Review);
        if matches!(state.kind, TakeoverKind::Sessions | TakeoverKind::Settings) || localmind_review
        {
            let total = if state.kind == TakeoverKind::Sessions {
                state.sessions.len()
            } else if localmind_review {
                state
                    .localmind
                    .as_ref()
                    .map_or(0, |localmind| localmind.data.review.len())
            } else {
                filtered_setting_indices(state).len()
            };
            state.selected = state
                .selected
                .saturating_add_signed(delta)
                .min(total.saturating_sub(1));
            keep_selection_visible(&mut state.scroll, state.selected, total, viewport_rows);
            return;
        }
        if state.kind == TakeoverKind::Diff {
            if state.diff_pane == DiffPane::Files {
                state.selected_file = state
                    .selected_file
                    .saturating_add_signed(delta)
                    .min(state.diff_files.len().saturating_sub(1));
                state.selected = first_diff_line(state.diff_files.get(state.selected_file));
                state.scroll = 0;
                keep_selection_visible(
                    &mut state.file_scroll,
                    state.selected_file,
                    state.diff_files.len(),
                    viewport_rows,
                );
            } else {
                state.selected = move_diff_line(
                    state.diff_files.get(state.selected_file),
                    state.selected,
                    delta,
                );
                let line_count = state
                    .diff_files
                    .get(state.selected_file)
                    .map_or(0, |file| file.lines.len());
                keep_selection_visible(
                    &mut state.scroll,
                    state.selected,
                    line_count,
                    viewport_rows,
                );
            }
            return;
        }
        let maximum = total_rows.saturating_sub(viewport_rows);
        state.scroll = state.scroll.saturating_add_signed(delta).min(maximum);
    }

    pub fn scroll_takeover_to(&mut self, start: usize, total_rows: usize, viewport_rows: usize) {
        let Some(state) = self.active_body_state_mut() else {
            return;
        };
        let localmind_review = state.kind == TakeoverKind::LocalMind
            && state
                .localmind
                .as_ref()
                .is_some_and(|localmind| localmind.section == LocalMindSection::Review);
        if matches!(state.kind, TakeoverKind::Sessions | TakeoverKind::Settings) || localmind_review
        {
            let total = if state.kind == TakeoverKind::Sessions {
                state.sessions.len()
            } else if localmind_review {
                state
                    .localmind
                    .as_ref()
                    .map_or(0, |localmind| localmind.data.review.len())
            } else {
                filtered_setting_indices(state).len()
            };
            state.scroll = start.min(total_rows.saturating_sub(viewport_rows));
            state.selected = start.min(total.saturating_sub(1));
            return;
        }
        if state.kind == TakeoverKind::Diff {
            if state.diff_pane == DiffPane::Files {
                state.file_scroll = start.min(total_rows.saturating_sub(viewport_rows));
                state.selected_file = start.min(state.diff_files.len().saturating_sub(1));
                state.selected = first_diff_line(state.diff_files.get(state.selected_file));
                state.scroll = 0;
            } else {
                state.scroll = start.min(total_rows.saturating_sub(viewport_rows));
                state.selected =
                    diff_line_at_or_after(state.diff_files.get(state.selected_file), start);
            }
            return;
        }
        state.scroll = start.min(total_rows.saturating_sub(viewport_rows));
    }

    #[must_use]
    pub fn shell_mode(&self) -> bool {
        self.editor.is_shell_mode()
    }

    #[must_use]
    pub(crate) fn completion(&self) -> Option<CompletionView<'_>> {
        let Some(InputOverlay::Completion(state)) = &self.input_overlay else {
            return None;
        };
        Some(CompletionView {
            kind: state.kind,
            items: &state.items,
            selected: state.selected,
            loading: state.kind == CompletionKind::File && !self.workspace_files_ready,
        })
    }

    fn dismiss_input_overlay(&mut self) {
        if self.projections.active().timeline_search.is_some() {
            self.projections.active_mut().timeline_search = None;
        } else {
            self.input_overlay = None;
        }
    }

    fn cancel_input_overlay(&mut self) {
        if let Some(state) = self.projections.active_mut().timeline_search.take() {
            self.editor.restore_snapshot(state.original_draft);
        } else {
            self.input_overlay = None;
        }
    }

    pub fn seed_history(&mut self, history: Vec<String>) {
        self.editor.seed_history(history);
    }

    /// Clear the current conversation projection while retaining host/session
    /// configuration, completion catalogs and durable prompt history.
    pub fn clear_conversation(&mut self) {
        self.close_theme_picker(true);
        self.escape_armed_at = None;
        self.projections.active_mut().clear_conversation();
        self.takeover = None;
        self.quick_help = false;
        self.input_overlay = None;
        self.external_edit_snapshot = None;
        // The reset installed a fresh (default-visible) timeline; reapply the
        // host-level reasoning visibility so hiding survives a clear/new session.
        self.reapply_reasoning_visibility();
    }

    #[must_use]
    pub const fn has_stashed_draft(&self) -> bool {
        self.stashed_draft.is_some()
    }

    /// A stash belongs to the current in-memory session and is not persisted.
    pub fn clear_stashed_draft(&mut self) {
        self.stashed_draft = None;
    }

    /// Record whether an explicit exit command requested a restored-buffer
    /// transcript. Keyboard exits always leave this at the safe summary-only
    /// default.
    pub fn request_exit(&mut self, print_transcript: bool) {
        self.print_transcript_on_exit = print_transcript;
        self.exit_requested = true;
    }

    #[must_use]
    pub const fn print_transcript_on_exit(&self) -> bool {
        self.print_transcript_on_exit
    }

    pub fn set_command_catalog(&mut self, commands: impl IntoIterator<Item = CompletionCommand>) {
        self.command_catalog = commands
            .into_iter()
            .map(|command| CompletionCommand {
                name: sanitize_text(&command.name),
                description: sanitize_text(&command.description),
            })
            .filter(|command| !command.name.is_empty())
            .collect();
    }

    /// Install the truthful values for one command-specific second-level
    /// picker. Execution remains a host responsibility after the completed
    /// slash line is submitted.
    pub fn set_command_values(
        &mut self,
        command: impl Into<String>,
        values: impl IntoIterator<Item = CompletionCommand>,
    ) {
        let command = sanitize_text(&command.into());
        let values = values
            .into_iter()
            .map(|value| CompletionItem {
                name: sanitize_text(&value.name),
                description: sanitize_text(&value.description),
            })
            .filter(|value| !value.name.is_empty())
            .collect();
        self.command_values.insert(command, values);
    }

    pub fn set_workspace_files(&mut self, files: impl IntoIterator<Item = String>) {
        self.workspace_files = files
            .into_iter()
            .map(|path| sanitize_text(&path).replace('\\', "/"))
            .filter(|path| !path.is_empty())
            .collect();
        self.workspace_files.sort();
        self.workspace_files.dedup();
        self.workspace_files_ready = true;
        if matches!(self.input_overlay, Some(InputOverlay::Completion(_))) {
            self.refresh_or_open_completion();
        }
    }

    /// The surface that currently owns input and therefore blocks an image
    /// attach, or `None` when the composer can accept one. This is the single
    /// ownership authority; callers read the reason rather than re-deriving it.
    #[must_use]
    pub fn image_attach_block(&self) -> Option<ImageAttachBlock> {
        if self.focus != Focus::Composer {
            Some(ImageAttachBlock::NotComposer)
        } else if self.dialog.is_some() {
            Some(ImageAttachBlock::Dialog)
        } else if matches!(
            self.active_body(),
            ActiveBody::Takeover | ActiveBody::LocalMind
        ) {
            Some(ImageAttachBlock::Takeover)
        } else if self.theme_picker.is_some() {
            Some(ImageAttachBlock::ThemePicker)
        } else if self.has_input_overlay() {
            Some(ImageAttachBlock::InputOverlay)
        } else if self.shell_mode() {
            Some(ImageAttachBlock::ShellMode)
        } else {
            None
        }
    }

    /// Attach an already-validated image to the ordinary composer. Overlays and
    /// dialogs own input while open, so a host-side Ctrl+V cannot bypass their
    /// containment contract; `image_attach_block` reports which one declined.
    pub fn attach_image(
        &mut self,
        media_type: impl Into<String>,
        data: impl Into<String>,
        byte_len: usize,
    ) -> Option<String> {
        if self.image_attach_block().is_some() {
            return None;
        }
        self.exit_armed = false;
        self.quick_help = false;
        Some(self.editor.insert_image(media_type, data, byte_len))
    }

    /// Placeholder-only text handed to an external editor after Ctrl+G.
    /// Opaque paste and image payloads remain in the private snapshot.
    #[must_use]
    pub fn external_edit_text(&self) -> Option<&str> {
        self.external_edit_snapshot
            .as_ref()
            .map(EditorSnapshot::text)
    }

    /// Complete an external-edit round trip. An unchanged file restores the
    /// exact snapshot (including atomic units and caret); any real edit becomes
    /// ordinary text so modified placeholders cannot resend stale payloads.
    pub fn finish_external_edit(&mut self, edited: Option<String>) {
        let Some(snapshot) = self.external_edit_snapshot.take() else {
            return;
        };
        let Some(edited) = edited else {
            self.editor.restore_snapshot(snapshot);
            return;
        };
        let mut edited = sanitize_text(&edited);
        if edited
            .strip_suffix('\n')
            .is_some_and(|without_newline| without_newline == snapshot.text())
        {
            edited.pop();
        }
        if edited == snapshot.text() {
            self.editor.restore_snapshot(snapshot);
        } else {
            let shell_mode = snapshot.shell_mode();
            self.editor.replace_draft(edited);
            if shell_mode {
                self.editor.enter_shell_mode();
            }
        }
    }

    /// Add one submitted prompt at its stable transcript position. The host
    /// supplies presentation-only trailing metadata such as a local time.
    pub fn append_prompt(
        &mut self,
        text: impl Into<String>,
        trailing: Option<String>,
        pending: bool,
    ) -> Option<ItemId> {
        self.projections.active_mut().timeline.follow_bottom();
        let id = self
            .projections
            .active_mut()
            .timeline
            .push(ItemKind::User, text)?;
        let _ = self
            .projections
            .active_mut()
            .timeline
            .set_trailing(id, trailing);
        let _ = self
            .projections
            .active_mut()
            .timeline
            .set_pending(id, pending);
        if pending
            && matches!(self.projections.active().work, WorkState::Busy { .. })
            && self.projections.active().active_insert_before.is_none()
        {
            self.projections.active_mut().active_insert_before = Some(id);
        }
        Some(id)
    }

    pub fn activate_prompt(&mut self, id: ItemId) -> bool {
        self.projections.active_mut().timeline.follow_bottom();
        self.projections
            .active_mut()
            .timeline
            .set_pending(id, false)
    }

    /// Activate a queued prompt in one named collaboration projection without
    /// changing the shared composer target.
    #[must_use]
    pub fn activate_prompt_for(&mut self, peer: PeerPane, id: ItemId) -> bool {
        let Some(projection) = self.projections.projection_mut(peer) else {
            return false;
        };
        projection.timeline.follow_bottom();
        projection.timeline.set_pending(id, false)
    }

    /// Append a retained, inspect-and-copy-only result into one named collaboration
    /// projection, toned by outcome. Returns `false` for the ordinary single-session
    /// model without mutating anything. Text is sanitized by the timeline like every
    /// other row.
    #[must_use]
    pub fn append_result_for(&mut self, peer: PeerPane, text: String, tone: ResultTone) -> bool {
        let Some(projection) = self.projections.projection_mut(peer) else {
            return false;
        };
        projection.timeline.follow_bottom();
        projection.timeline.push_result(text, tone).is_some()
    }

    /// Insert a stable compact user-shell row. Pending rows intentionally carry
    /// no running activity until their ordered queue position activates.
    pub fn append_shell(&mut self, command: &UserShellCommand, pending: bool) -> Option<ItemId> {
        self.projections.active_mut().timeline.follow_bottom();
        let id = self
            .projections
            .active_mut()
            .timeline
            .push(ItemKind::Shell, format!("Shell {}", command.as_str()))?;
        if pending
            && matches!(self.projections.active().work, WorkState::Busy { .. })
            && self.projections.active().active_insert_before.is_none()
        {
            self.projections.active_mut().active_insert_before = Some(id);
        }
        Some(id)
    }

    pub fn activate_shell(&mut self, id: ItemId) -> bool {
        self.projections.active_mut().timeline.follow_bottom();
        self.projections
            .active_mut()
            .timeline
            .set_activity(id, Some(ActivityState::Running))
    }

    pub fn finish_shell(
        &mut self,
        id: ItemId,
        command: &UserShellCommand,
        output: &UserShellOutput,
    ) -> bool {
        let line_count = output.lines.len();
        let unit = if line_count == 1 { "line" } else { "lines" };
        let command = command.as_str().replace(['\r', '\n'], " ");
        let mut text = format!("Shell {command} {line_count} {unit}");
        for line in &output.lines {
            text.push('\n');
            text.push_str(line);
        }
        if !self
            .projections
            .active_mut()
            .timeline
            .replace_text(id, text)
        {
            return false;
        }
        self.projections.active_mut().timeline.set_activity(
            id,
            Some(if output.success {
                ActivityState::Success
            } else {
                ActivityState::Error
            }),
        )
    }

    pub fn require_workspace_trust(&mut self, path: impl Into<String>) {
        self.claim_dialog_focus();
        self.dialog_peer = None;
        self.dialog = Some(DialogState::Trust(TrustDialog {
            path: bounded_inline_text(&path.into(), MAX_TOOL_DETAIL_BYTES),
            selected: 0,
            path_selection: None,
        }));
    }

    #[must_use]
    pub fn workspace_trust_pending(&self) -> bool {
        matches!(self.dialog, Some(DialogState::Trust(_)))
    }

    #[must_use]
    pub(crate) fn trust(&self) -> Option<TrustView<'_>> {
        let Some(DialogState::Trust(trust)) = &self.dialog else {
            return None;
        };
        Some(TrustView {
            path: &trust.path,
            selected: trust.selected,
            path_selection: trust.path_selection.as_ref().map(|selection| {
                let (start, end) = if selection.anchor <= selection.focus {
                    (selection.anchor, selection.focus)
                } else {
                    (selection.focus, selection.anchor)
                };
                TrustPathSelectionView {
                    source: &selection.source,
                    start,
                    end,
                }
            }),
        })
    }

    pub fn select_trust_option(&mut self, index: usize) {
        let Some(DialogState::Trust(trust)) = &mut self.dialog else {
            return;
        };
        trust.selected = index.min(2);
        trust.path_selection = None;
    }

    pub fn start_trust_path_selection(&mut self, source: String, byte: usize) {
        let Some(DialogState::Trust(trust)) = &mut self.dialog else {
            return;
        };
        let byte = previous_grapheme_boundary(&source, byte);
        trust.path_selection = Some(TrustPathSelection {
            source,
            anchor: byte,
            focus: byte,
        });
    }

    pub fn extend_trust_path_selection(&mut self, source: &str, byte: usize) {
        let Some(DialogState::Trust(trust)) = &mut self.dialog else {
            return;
        };
        let Some(selection) = &mut trust.path_selection else {
            return;
        };
        if selection.source != source {
            trust.path_selection = None;
            return;
        }
        selection.focus = previous_grapheme_boundary(source, byte);
    }

    #[must_use]
    pub fn trust_path_selection_active(&self) -> bool {
        matches!(
            &self.dialog,
            Some(DialogState::Trust(TrustDialog {
                path_selection: Some(_),
                ..
            }))
        )
    }

    #[must_use]
    fn trust_selected_text(&self) -> Option<String> {
        let Some(DialogState::Trust(trust)) = &self.dialog else {
            return None;
        };
        let selection = trust.path_selection.as_ref()?;
        let (start, end) = if selection.anchor <= selection.focus {
            (selection.anchor, selection.focus)
        } else {
            (selection.focus, selection.anchor)
        };
        (start < end).then(|| selection.source[start..end].to_string())
    }

    pub fn handle_trust_input(&mut self, action: InputAction) -> TrustAction {
        let Some(DialogState::Trust(trust)) = &mut self.dialog else {
            return TrustAction::None;
        };
        match action {
            InputAction::MoveUp => {
                trust.selected = trust.selected.saturating_sub(1);
                trust.path_selection = None;
                TrustAction::None
            }
            InputAction::MoveDown => {
                trust.selected = trust.selected.saturating_add(1).min(2);
                trust.path_selection = None;
                TrustAction::None
            }
            InputAction::Submit | InputAction::AcceptCompletion => match trust.selected {
                0 => TrustAction::ContinueSession,
                1 => TrustAction::Remember,
                _ => TrustAction::Deny,
            },
            InputAction::Escape => TrustAction::Deny,
            _ => TrustAction::None,
        }
    }

    pub fn request_approval(
        &mut self,
        tool: impl Into<String>,
        target: impl Into<String>,
        risk_class: impl Into<String>,
    ) {
        let peer = self.active_pair_pane();
        self.install_approval(peer, tool, target, risk_class);
    }

    /// Opens an approval attributed to one named peer and makes that peer the
    /// shared composer target. Returns `false` for a single session or while a
    /// dialog is already open.
    #[must_use]
    pub fn request_approval_for(
        &mut self,
        peer: PeerPane,
        tool: impl Into<String>,
        target: impl Into<String>,
        risk_class: impl Into<String>,
    ) -> bool {
        if self.dialog.is_some() || self.projections.projection(peer).is_none() {
            return false;
        }
        if self.active_pair_pane() != Some(peer) && !self.select_pair_pane(peer) {
            return false;
        }
        self.install_approval(Some(peer), tool, target, risk_class);
        true
    }

    fn install_approval(
        &mut self,
        peer: Option<PeerPane>,
        tool: impl Into<String>,
        target: impl Into<String>,
        risk_class: impl Into<String>,
    ) {
        self.claim_dialog_focus();
        self.dialog_peer = peer;
        self.dialog = Some(DialogState::Approval {
            tool: sanitize_inline(&tool.into()),
            target: sanitize_inline(&target.into()),
            risk_class: sanitize_inline(&risk_class.into()),
        });
    }

    pub fn request_question(
        &mut self,
        header: Option<String>,
        question: impl Into<String>,
        options: impl IntoIterator<Item = QuestionOption>,
        multi_select: bool,
        index: usize,
        total: usize,
    ) {
        let peer = self.active_pair_pane();
        self.install_question(peer, header, question, options, multi_select, index, total);
    }

    /// Opens a question attributed to one named peer and makes that peer the
    /// shared composer target. Returns `false` for a single session or while a
    /// dialog is already open.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn request_question_for(
        &mut self,
        peer: PeerPane,
        header: Option<String>,
        question: impl Into<String>,
        options: impl IntoIterator<Item = QuestionOption>,
        multi_select: bool,
        index: usize,
        total: usize,
    ) -> bool {
        if self.dialog.is_some() || self.projections.projection(peer).is_none() {
            return false;
        }
        if self.active_pair_pane() != Some(peer) && !self.select_pair_pane(peer) {
            return false;
        }
        self.install_question(
            Some(peer),
            header,
            question,
            options,
            multi_select,
            index,
            total,
        );
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn install_question(
        &mut self,
        peer: Option<PeerPane>,
        header: Option<String>,
        question: impl Into<String>,
        options: impl IntoIterator<Item = QuestionOption>,
        multi_select: bool,
        index: usize,
        total: usize,
    ) {
        self.claim_dialog_focus();
        self.dialog_peer = peer;
        let options = options
            .into_iter()
            .map(|option| QuestionOption {
                label: bounded_inline_text(&option.label, 1024),
                description: option
                    .description
                    .map(|description| bounded_inline_text(&description, 1024))
                    .filter(|description| !description.is_empty()),
            })
            .filter(|option| !option.label.is_empty())
            .take(8)
            .collect::<Vec<_>>();
        let checked = vec![false; options.len()];
        self.dialog = Some(DialogState::Question(QuestionDialog {
            header: header
                .map(|header| bounded_inline_text(&header, 128))
                .filter(|header| !header.is_empty()),
            question: bounded_inline_text(&question.into(), MAX_TOOL_DETAIL_BYTES),
            options,
            selected: 0,
            checked,
            multi_select,
            editing_other: false,
            other: String::new(),
            other_cursor: 0,
            index: index.max(1),
            total: total.max(1),
        }));
    }

    #[must_use]
    pub(crate) fn question(&self) -> Option<QuestionView<'_>> {
        let Some(DialogState::Question(question)) = &self.dialog else {
            return None;
        };
        Some(QuestionView {
            header: question.header.as_deref(),
            question: &question.question,
            options: &question.options,
            selected: question.selected,
            checked: &question.checked,
            multi_select: question.multi_select,
            editing_other: question.editing_other,
            other: &question.other,
            other_cursor: question.other_cursor,
            index: question.index,
            total: question.total,
        })
    }

    pub fn select_question_option(&mut self, index: usize) {
        let Some(DialogState::Question(question)) = &mut self.dialog else {
            return;
        };
        question.selected = index.min(question.options.len());
        question.editing_other = false;
    }

    pub fn handle_question_input(&mut self, action: InputAction) -> QuestionAction {
        let Some(DialogState::Question(question)) = &mut self.dialog else {
            return QuestionAction::None;
        };
        if question.editing_other {
            match action {
                InputAction::Escape => question.editing_other = false,
                InputAction::Insert(text) | InputAction::Paste(text) => {
                    let text = sanitize_inline(&text);
                    let remaining = MAX_TOOL_DETAIL_BYTES.saturating_sub(question.other.len());
                    let end = previous_char_boundary(&text, remaining);
                    question
                        .other
                        .insert_str(question.other_cursor, &text[..end]);
                    question.other_cursor = question.other_cursor.saturating_add(end);
                }
                InputAction::Backspace => {
                    if question.other_cursor > 0 {
                        let start = previous_grapheme_boundary(
                            &question.other,
                            question.other_cursor.saturating_sub(1),
                        );
                        question.other.drain(start..question.other_cursor);
                        question.other_cursor = start;
                    }
                }
                InputAction::Delete => {
                    if question.other_cursor < question.other.len() {
                        let end = question.other[question.other_cursor..]
                            .graphemes(true)
                            .next()
                            .map_or(question.other.len(), |grapheme| {
                                question.other_cursor.saturating_add(grapheme.len())
                            });
                        question.other.drain(question.other_cursor..end);
                    }
                }
                InputAction::MoveLeft => {
                    question.other_cursor = previous_grapheme_boundary(
                        &question.other,
                        question.other_cursor.saturating_sub(1),
                    );
                }
                InputAction::MoveRight | InputAction::ForwardCharOrSearch => {
                    question.other_cursor = question.other[question.other_cursor..]
                        .graphemes(true)
                        .next()
                        .map_or(question.other.len(), |grapheme| {
                            question.other_cursor.saturating_add(grapheme.len())
                        });
                }
                InputAction::MoveVisualStart
                | InputAction::MoveLineStart
                | InputAction::MoveTextStart => question.other_cursor = 0,
                InputAction::MoveVisualEnd
                | InputAction::MoveLineEnd
                | InputAction::MoveTextEnd => question.other_cursor = question.other.len(),
                InputAction::Submit => {
                    let answer = question.other.trim();
                    if !answer.is_empty() {
                        return QuestionAction::Submit(QuestionResponse::Other(answer.to_string()));
                    }
                }
                _ => {}
            }
            return QuestionAction::None;
        }
        match action {
            InputAction::MoveUp => {
                question.selected = question.selected.saturating_sub(1);
                QuestionAction::None
            }
            InputAction::MoveDown => {
                question.selected = question
                    .selected
                    .saturating_add(1)
                    .min(question.options.len());
                QuestionAction::None
            }
            InputAction::Insert(text)
                if text == " "
                    && question.multi_select
                    && question.selected < question.options.len() =>
            {
                if let Some(checked) = question.checked.get_mut(question.selected) {
                    *checked = !*checked;
                }
                QuestionAction::None
            }
            InputAction::Submit | InputAction::AcceptCompletion => {
                if question.selected < question.options.len() {
                    let selected = if question.multi_select {
                        let checked = question
                            .options
                            .iter()
                            .zip(&question.checked)
                            .filter(|(_, checked)| **checked)
                            .map(|(option, _)| option.label.clone())
                            .collect::<Vec<_>>();
                        if checked.is_empty() {
                            vec![question.options[question.selected].label.clone()]
                        } else {
                            checked
                        }
                    } else {
                        vec![question.options[question.selected].label.clone()]
                    };
                    QuestionAction::Submit(QuestionResponse::Selected(selected))
                } else {
                    question.editing_other = true;
                    question.other_cursor = question.other.len();
                    QuestionAction::None
                }
            }
            InputAction::Escape => QuestionAction::Cancel,
            _ => QuestionAction::None,
        }
    }

    pub fn clear_dialog(&mut self) {
        self.dialog = None;
        self.dialog_peer = None;
    }

    /// Returns the immutable peer attribution of the current dialog.
    #[must_use]
    pub const fn dialog_peer(&self) -> Option<PeerPane> {
        self.dialog_peer
    }

    /// The model label of the current dialog's origin peer, borrowed from that peer's
    /// named projection. `None` when no dialog peer is attributed (ordinary single
    /// chat). The renderer sanitizes it; the ask channel never carries this string.
    #[must_use]
    pub(crate) fn dialog_peer_model(&self) -> Option<&str> {
        let peer = self.dialog_peer()?;
        self.projections
            .projection(peer)
            .map(|projection| projection.header.model.as_str())
    }

    fn claim_dialog_focus(&mut self) {
        self.escape_armed_at = None;
        self.quick_help = false;
        if self.theme_picker.is_some() {
            self.close_theme_picker(true);
        }
        self.takeover = None;
        if self.has_input_overlay() {
            self.cancel_input_overlay();
        }
    }

    pub fn disarm_exit(&mut self) {
        self.exit_armed = false;
        self.escape_armed_at = None;
    }

    pub fn set_copy_on_select(&mut self, enabled: bool) {
        self.copy_on_select = enabled;
        self.sync_setting_values();
    }

    #[must_use]
    pub const fn copy_on_select(&self) -> bool {
        self.copy_on_select
    }

    /// Captures the environment-seeded session defaults used by the settings
    /// takeover's non-persistent reset action.
    pub fn capture_setting_defaults(&mut self) {
        self.default_copy_on_select = self.copy_on_select;
        self.default_theme = self.theme;
        self.sync_setting_values();
    }

    #[must_use]
    pub const fn copy_on_select_is_default(&self) -> bool {
        self.copy_on_select == self.default_copy_on_select
    }

    #[must_use]
    pub fn theme_is_default(&self) -> bool {
        self.theme == self.default_theme
    }

    fn sync_setting_values(&mut self) {
        let Some(state) = self
            .takeover
            .as_mut()
            .filter(|state| state.kind == TakeoverKind::Settings)
        else {
            return;
        };
        for setting in &mut state.settings {
            match setting.edit {
                Some(SettingEdit::CopyOnSelect) => {
                    setting.value = if self.copy_on_select { "On" } else { "Off" }.to_string();
                    setting.is_default = self.copy_on_select == self.default_copy_on_select;
                }
                Some(SettingEdit::Theme) => {
                    setting.value = self.theme.display_name().to_string();
                    setting.is_default = self.theme == self.default_theme;
                }
                None => {}
            }
        }
    }

    fn open_reverse_history(&mut self) {
        self.quick_help = false;
        self.input_overlay = Some(InputOverlay::ReverseHistory(ReverseHistoryState {
            query: String::new(),
            match_index: None,
            original_draft: self.editor.snapshot(),
        }));
        self.refresh_reverse_history(None);
    }

    /// Open timeline search, seeding the query (empty for a blank search). The
    /// query is sanitized and newline-flattened before it is stored.
    pub fn open_timeline_search(&mut self, query: String) {
        self.quick_help = false;
        let original_draft = self.editor.snapshot();
        self.editor.replace_draft(String::new());
        self.projections.active_mut().timeline_search = Some(TimelineSearchState {
            query: sanitize_text(&query).replace(['\r', '\n'], " "),
            matches: Vec::new(),
            selected: None,
            original_draft,
        });
        self.refresh_timeline_search();
    }

    fn handle_overlay_input(&mut self, action: InputAction) -> AppCommand {
        if matches!(self.input_overlay, Some(InputOverlay::Completion(_))) {
            return self.handle_completion_input(action);
        }
        if self.projections.active().timeline_search.is_some() {
            return self.handle_timeline_search_input(action);
        }
        match action {
            InputAction::Escape => self.dismiss_input_overlay(),
            InputAction::Submit => {
                self.dismiss_input_overlay();
                return self.submit_editor();
            }
            InputAction::OpenReverseHistory | InputAction::MoveUp => {
                let before = self.reverse_history_index();
                self.refresh_reverse_history(before);
            }
            InputAction::MoveDown => self.advance_reverse_history(),
            InputAction::Insert(text) | InputAction::Paste(text) => {
                let text = sanitize_text(&text).replace(['\r', '\n'], " ");
                if let Some(InputOverlay::ReverseHistory(state)) = &mut self.input_overlay {
                    state.query.push_str(&text);
                    state.match_index = None;
                }
                self.refresh_reverse_history(None);
            }
            InputAction::Backspace => {
                if let Some(InputOverlay::ReverseHistory(state)) = &mut self.input_overlay {
                    if let Some((byte, _)) = state.query.grapheme_indices(true).next_back() {
                        state.query.truncate(byte);
                    }
                    state.match_index = None;
                }
                self.refresh_reverse_history(None);
            }
            InputAction::CancelOrExit
            | InputAction::StashOrPop
            | InputAction::CyclePeer
            | InputAction::NavigateTimeline(_)
            | InputAction::Delete
            | InputAction::MoveLeft
            | InputAction::MoveRight
            | InputAction::ForwardCharOrSearch
            | InputAction::MoveWordLeft
            | InputAction::MoveWordRight
            | InputAction::MoveVisualStart
            | InputAction::MoveVisualEnd
            | InputAction::MoveLineStart
            | InputAction::MoveLineEnd
            | InputAction::MoveTextStart
            | InputAction::MoveTextEnd
            | InputAction::DeleteWordLeft
            | InputAction::DeleteToLineStart
            | InputAction::DeleteToLineEnd
            | InputAction::OpenExternalEditor
            | InputAction::PreviousLocalMindSection
            | InputAction::AcceptCompletion => {}
        }
        AppCommand::None
    }

    fn handle_takeover_input(&mut self, action: InputAction) -> AppCommand {
        match action {
            InputAction::Escape => {
                if self.clear_settings_query() {
                    return AppCommand::None;
                }
                self.takeover = None;
                AppCommand::None
            }
            InputAction::PreviousLocalMindSection => AppCommand::None,
            InputAction::MoveUp => AppCommand::NavigateTakeover(TakeoverNavigation::LineUp),
            InputAction::MoveDown => AppCommand::NavigateTakeover(TakeoverNavigation::LineDown),
            InputAction::NavigateTimeline(TimelineNavigation::PageUp) => {
                AppCommand::NavigateTakeover(TakeoverNavigation::PageUp)
            }
            InputAction::NavigateTimeline(TimelineNavigation::PageDown) => {
                AppCommand::NavigateTakeover(TakeoverNavigation::PageDown)
            }
            InputAction::MoveLeft => {
                if let Some(state) = self.takeover.as_mut() {
                    if state.kind == TakeoverKind::Diff && state.tree_visible {
                        state.diff_pane = DiffPane::Files;
                    }
                }
                AppCommand::None
            }
            InputAction::MoveRight => {
                if let Some(state) = self.takeover.as_mut() {
                    if state.kind == TakeoverKind::Diff {
                        state.diff_pane = DiffPane::Content;
                    }
                }
                AppCommand::None
            }
            InputAction::Insert(text) if text.eq_ignore_ascii_case("t") => {
                if let Some(state) = self.takeover.as_mut() {
                    if state.kind == TakeoverKind::Diff {
                        state.tree_visible = !state.tree_visible;
                        if !state.tree_visible {
                            state.diff_pane = DiffPane::Content;
                        }
                    }
                }
                if self
                    .takeover
                    .as_ref()
                    .is_some_and(|state| state.kind == TakeoverKind::Settings)
                {
                    self.append_settings_query(&text);
                }
                AppCommand::None
            }
            InputAction::Insert(text) | InputAction::Paste(text)
                if self
                    .takeover
                    .as_ref()
                    .is_some_and(|state| state.kind == TakeoverKind::Settings) =>
            {
                self.append_settings_query(&text);
                AppCommand::None
            }
            InputAction::Backspace
                if self
                    .takeover
                    .as_ref()
                    .is_some_and(|state| state.kind == TakeoverKind::Settings) =>
            {
                self.backspace_settings_query();
                AppCommand::None
            }
            InputAction::OpenReverseHistory
                if self
                    .takeover
                    .as_ref()
                    .is_some_and(|state| state.kind == TakeoverKind::Settings) =>
            {
                self.edit_selected_setting(true);
                AppCommand::None
            }
            InputAction::Submit | InputAction::AcceptCompletion => {
                if self
                    .takeover
                    .as_ref()
                    .is_some_and(|state| state.kind == TakeoverKind::Settings)
                {
                    self.edit_selected_setting(false);
                    return AppCommand::None;
                }
                let selection = self.takeover.as_ref().and_then(|state| {
                    (state.kind == TakeoverKind::Sessions)
                        .then(|| state.sessions.get(state.selected))
                        .flatten()
                        .map(|entry| SessionSelection {
                            selector: entry.selector.clone(),
                        })
                });
                if selection.is_some() {
                    self.takeover = None;
                }
                selection.map_or(AppCommand::None, AppCommand::ActivateSession)
            }
            InputAction::CancelOrExit
            | InputAction::OpenReverseHistory
            | InputAction::StashOrPop
            | InputAction::CyclePeer
            | InputAction::Insert(_)
            | InputAction::Paste(_)
            | InputAction::Backspace
            | InputAction::Delete
            | InputAction::ForwardCharOrSearch
            | InputAction::MoveWordLeft
            | InputAction::MoveWordRight
            | InputAction::MoveVisualStart
            | InputAction::MoveVisualEnd
            | InputAction::MoveLineStart
            | InputAction::MoveLineEnd
            | InputAction::MoveTextStart
            | InputAction::MoveTextEnd
            | InputAction::DeleteWordLeft
            | InputAction::DeleteToLineStart
            | InputAction::DeleteToLineEnd
            | InputAction::OpenExternalEditor => AppCommand::None,
        }
    }

    fn handle_localmind_input(&mut self, action: InputAction) -> AppCommand {
        match action {
            InputAction::Escape => {
                if let Some(localmind) = self
                    .localmind_tab
                    .as_mut()
                    .and_then(|state| state.localmind.as_mut())
                    .filter(|state| state.editing_reviewer)
                {
                    localmind.editing_reviewer = false;
                } else {
                    let _ = self.activate_tab(TabId::Session);
                }
                AppCommand::None
            }
            InputAction::PreviousLocalMindSection => {
                self.cycle_localmind_section(false);
                AppCommand::None
            }
            InputAction::MoveUp => AppCommand::NavigateTakeover(TakeoverNavigation::LineUp),
            InputAction::MoveDown => AppCommand::NavigateTakeover(TakeoverNavigation::LineDown),
            InputAction::NavigateTimeline(TimelineNavigation::PageUp) => {
                AppCommand::NavigateTakeover(TakeoverNavigation::PageUp)
            }
            InputAction::NavigateTimeline(TimelineNavigation::PageDown) => {
                AppCommand::NavigateTakeover(TakeoverNavigation::PageDown)
            }
            InputAction::Insert(text)
                if self
                    .localmind_tab
                    .as_ref()
                    .and_then(|state| state.localmind.as_ref())
                    .is_some_and(|state| state.editing_reviewer) =>
            {
                self.append_localmind_reviewer(&text);
                AppCommand::None
            }
            InputAction::Paste(text)
                if self
                    .localmind_tab
                    .as_ref()
                    .and_then(|state| state.localmind.as_ref())
                    .is_some_and(|state| state.editing_reviewer) =>
            {
                self.append_localmind_reviewer(&text);
                AppCommand::None
            }
            InputAction::Backspace
                if self
                    .localmind_tab
                    .as_ref()
                    .and_then(|state| state.localmind.as_ref())
                    .is_some_and(|state| state.editing_reviewer) =>
            {
                self.backspace_localmind_reviewer();
                AppCommand::None
            }
            InputAction::Insert(text)
                if text.eq_ignore_ascii_case("i") && self.localmind_review_is_active() =>
            {
                if let Some(localmind) = self
                    .localmind_tab
                    .as_mut()
                    .and_then(|state| state.localmind.as_mut())
                    .filter(|state| state.section == LocalMindSection::Review)
                {
                    localmind.editing_reviewer = true;
                }
                AppCommand::None
            }
            InputAction::Insert(text)
                if text.eq_ignore_ascii_case("a") && self.localmind_review_is_active() =>
            {
                self.localmind_review_intent(LocalMindReviewAction::Accept)
                    .map_or(AppCommand::None, AppCommand::LocalMindReview)
            }
            InputAction::Insert(text)
                if text.eq_ignore_ascii_case("r") && self.localmind_review_is_active() =>
            {
                self.localmind_review_intent(LocalMindReviewAction::Reject)
                    .map_or(AppCommand::None, AppCommand::LocalMindReview)
            }
            InputAction::Insert(text)
                if text.eq_ignore_ascii_case("p") && self.localmind_review_is_active() =>
            {
                self.localmind_review_intent(LocalMindReviewAction::Promote)
                    .map_or(AppCommand::None, AppCommand::LocalMindReview)
            }
            InputAction::AcceptCompletion => {
                self.cycle_localmind_section(true);
                AppCommand::None
            }
            InputAction::Submit => {
                if let Some(localmind) = self
                    .localmind_tab
                    .as_mut()
                    .and_then(|state| state.localmind.as_mut())
                    .filter(|state| state.editing_reviewer)
                {
                    if !localmind.reviewer.trim().is_empty() {
                        localmind.editing_reviewer = false;
                    }
                }
                AppCommand::None
            }
            InputAction::CancelOrExit
            | InputAction::OpenReverseHistory
            | InputAction::StashOrPop
            | InputAction::CyclePeer
            | InputAction::Insert(_)
            | InputAction::Paste(_)
            | InputAction::Backspace
            | InputAction::Delete
            | InputAction::MoveLeft
            | InputAction::MoveRight
            | InputAction::ForwardCharOrSearch
            | InputAction::MoveWordLeft
            | InputAction::MoveWordRight
            | InputAction::MoveVisualStart
            | InputAction::MoveVisualEnd
            | InputAction::MoveLineStart
            | InputAction::MoveLineEnd
            | InputAction::MoveTextStart
            | InputAction::MoveTextEnd
            | InputAction::DeleteWordLeft
            | InputAction::DeleteToLineStart
            | InputAction::DeleteToLineEnd
            | InputAction::OpenExternalEditor => AppCommand::None,
        }
    }

    fn handle_theme_picker_input(&mut self, action: InputAction) -> AppCommand {
        match action {
            InputAction::Escape => self.close_theme_picker(true),
            InputAction::MoveUp => self.move_theme_picker(-1),
            InputAction::MoveDown => self.move_theme_picker(1),
            InputAction::Submit | InputAction::AcceptCompletion => self.close_theme_picker(false),
            InputAction::CancelOrExit
            | InputAction::OpenReverseHistory
            | InputAction::StashOrPop
            | InputAction::CyclePeer
            | InputAction::NavigateTimeline(_)
            | InputAction::Insert(_)
            | InputAction::Paste(_)
            | InputAction::Backspace
            | InputAction::Delete
            | InputAction::MoveLeft
            | InputAction::MoveRight
            | InputAction::ForwardCharOrSearch
            | InputAction::MoveWordLeft
            | InputAction::MoveWordRight
            | InputAction::MoveVisualStart
            | InputAction::MoveVisualEnd
            | InputAction::MoveLineStart
            | InputAction::MoveLineEnd
            | InputAction::MoveTextStart
            | InputAction::MoveTextEnd
            | InputAction::DeleteWordLeft
            | InputAction::DeleteToLineStart
            | InputAction::DeleteToLineEnd
            | InputAction::OpenExternalEditor
            | InputAction::PreviousLocalMindSection => {}
        }
        AppCommand::None
    }

    fn move_theme_picker(&mut self, delta: isize) {
        let Some(picker) = self.theme_picker.as_ref() else {
            return;
        };
        let len = Theme::ALL.len();
        let selected = if delta < 0 {
            picker.selected.checked_sub(1).unwrap_or(len - 1)
        } else {
            (picker.selected + 1) % len
        };
        self.select_theme(selected);
    }

    fn close_theme_picker(&mut self, restore: bool) {
        let Some(picker) = self.theme_picker.take() else {
            return;
        };
        if restore {
            self.theme = picker.original;
        }
        self.takeover = picker.return_to.map(|state| *state);
        self.sync_setting_values();
    }

    fn handle_completion_input(&mut self, action: InputAction) -> AppCommand {
        match action {
            InputAction::Escape => self.dismiss_input_overlay(),
            InputAction::Submit | InputAction::AcceptCompletion => {
                return self.accept_completion();
            }
            InputAction::MoveUp => self.move_completion(-1),
            InputAction::MoveDown => self.move_completion(1),
            InputAction::Insert(text) => {
                let enters_command_values = matches!(
                    self.input_overlay,
                    Some(InputOverlay::Completion(CompletionState {
                        kind: CompletionKind::Command,
                        ..
                    }))
                ) && text.contains(char::is_whitespace);
                self.editor.insert(&text);
                if enters_command_values && self.open_command_values_for_draft() {
                    return AppCommand::None;
                }
                self.refresh_or_open_completion();
            }
            InputAction::Paste(text) => {
                self.editor.insert_paste(text);
                self.refresh_or_open_completion();
            }
            InputAction::Backspace => {
                self.editor.backspace();
                self.refresh_or_open_completion();
            }
            InputAction::Delete => {
                self.editor.delete();
                self.refresh_or_open_completion();
            }
            InputAction::MoveLeft => {
                self.editor.move_left();
                self.refresh_or_open_completion();
            }
            InputAction::MoveRight => {
                self.editor.move_right();
                self.refresh_or_open_completion();
            }
            InputAction::ForwardCharOrSearch => {
                self.editor.move_right();
                self.refresh_or_open_completion();
            }
            InputAction::MoveWordLeft => {
                self.editor.move_word_left();
                self.refresh_or_open_completion();
            }
            InputAction::MoveWordRight => {
                self.editor.move_word_right();
                self.refresh_or_open_completion();
            }
            InputAction::CancelOrExit
            | InputAction::OpenReverseHistory
            | InputAction::StashOrPop
            | InputAction::CyclePeer
            | InputAction::NavigateTimeline(_)
            | InputAction::MoveVisualStart
            | InputAction::MoveVisualEnd
            | InputAction::MoveLineStart
            | InputAction::MoveLineEnd
            | InputAction::MoveTextStart
            | InputAction::MoveTextEnd
            | InputAction::DeleteWordLeft
            | InputAction::DeleteToLineStart
            | InputAction::DeleteToLineEnd
            | InputAction::OpenExternalEditor
            | InputAction::PreviousLocalMindSection => {}
        }
        AppCommand::None
    }

    fn handle_timeline_search_input(&mut self, action: InputAction) -> AppCommand {
        match action {
            InputAction::Escape => self.cancel_input_overlay(),
            InputAction::Submit | InputAction::MoveDown => self.move_timeline_search(1),
            InputAction::MoveUp => self.move_timeline_search(-1),
            InputAction::Insert(text) | InputAction::Paste(text) => {
                let text = sanitize_text(&text).replace(['\r', '\n'], " ");
                if let Some(state) = &mut self.projections.active_mut().timeline_search {
                    state.query.push_str(&text);
                }
                self.refresh_timeline_search();
            }
            InputAction::Backspace => {
                if let Some(state) = &mut self.projections.active_mut().timeline_search {
                    if let Some((byte, _)) = state.query.grapheme_indices(true).next_back() {
                        state.query.truncate(byte);
                    }
                }
                self.refresh_timeline_search();
            }
            InputAction::CancelOrExit
            | InputAction::OpenReverseHistory
            | InputAction::StashOrPop
            | InputAction::CyclePeer
            | InputAction::NavigateTimeline(_)
            | InputAction::Delete
            | InputAction::MoveLeft
            | InputAction::MoveRight
            | InputAction::ForwardCharOrSearch
            | InputAction::MoveWordLeft
            | InputAction::MoveWordRight
            | InputAction::MoveVisualStart
            | InputAction::MoveVisualEnd
            | InputAction::MoveLineStart
            | InputAction::MoveLineEnd
            | InputAction::MoveTextStart
            | InputAction::MoveTextEnd
            | InputAction::DeleteWordLeft
            | InputAction::DeleteToLineStart
            | InputAction::DeleteToLineEnd
            | InputAction::OpenExternalEditor
            | InputAction::PreviousLocalMindSection
            | InputAction::AcceptCompletion => {}
        }
        AppCommand::None
    }

    fn refresh_timeline_search(&mut self) {
        Self::refresh_timeline_search_on(self.projections.active_mut());
    }

    fn refresh_timeline_search_on(projection: &mut SessionProjection) {
        let Some(state) = &projection.timeline_search else {
            return;
        };
        let query = state.query.clone();
        let previous = state
            .selected
            .and_then(|selected| state.matches.get(selected))
            .copied();
        let previous_order = previous.and_then(|point| {
            projection
                .timeline
                .items()
                .iter()
                .position(|item| item.id == point.item_id)
        });
        let matches = timeline_search_matches(&projection.timeline, &query);
        let selected = if matches.is_empty() {
            None
        } else if let Some(previous) = previous {
            matches
                .iter()
                .position(|point| point.item_id == previous.item_id)
                .or_else(|| {
                    previous_order.and_then(|previous_order| {
                        matches
                            .iter()
                            .enumerate()
                            .filter_map(|(match_index, point)| {
                                projection
                                    .timeline
                                    .items()
                                    .iter()
                                    .position(|item| item.id == point.item_id)
                                    .map(|item_index| {
                                        (
                                            item_index.abs_diff(previous_order),
                                            usize::MAX.saturating_sub(item_index),
                                            match_index,
                                        )
                                    })
                            })
                            .min()
                            .map(|(_, _, match_index)| match_index)
                    })
                })
                .or_else(|| matches.len().checked_sub(1))
        } else {
            matches.len().checked_sub(1)
        };
        let point = selected.and_then(|selected| matches.get(selected)).copied();
        if let Some(state) = &mut projection.timeline_search {
            state.matches = matches;
            state.selected = selected;
        }
        if let Some(point) = point {
            Self::reveal_timeline_search_point(projection, point);
        }
    }

    fn move_timeline_search(&mut self, delta: isize) {
        let projection = self.projections.active_mut();
        let point = {
            let Some(state) = &mut projection.timeline_search else {
                return;
            };
            let len = state.matches.len();
            if len == 0 {
                return;
            }
            let selected = state.selected.unwrap_or(len - 1);
            let next = if delta < 0 {
                selected.checked_sub(1).unwrap_or(len - 1)
            } else {
                (selected + 1) % len
            };
            state.selected = Some(next);
            state.matches[next]
        };
        Self::reveal_timeline_search_point(projection, point);
    }

    fn reveal_timeline_search_point(projection: &mut SessionProjection, point: ContentPoint) {
        let collapsed_tool = projection
            .timeline
            .item(point.item_id)
            .is_some_and(|item| item.kind == ItemKind::Tool && !item.expanded);
        if collapsed_tool {
            let _ = projection.timeline.set_expanded(point.item_id, true);
        }
        let _ = projection.timeline.hold_at(point);
    }

    fn refresh_or_open_completion(&mut self) {
        if self.editor.is_shell_mode() {
            if matches!(self.input_overlay, Some(InputOverlay::Completion(_))) {
                self.dismiss_input_overlay();
            }
            return;
        }
        if matches!(
            self.input_overlay,
            Some(InputOverlay::Completion(CompletionState {
                kind: CompletionKind::CommandValue,
                ..
            }))
        ) {
            self.refresh_command_value_completion();
            return;
        }
        let completion = self
            .editor
            .slash_token()
            .map(|token| (CompletionKind::Command, token))
            .or_else(|| {
                self.editor
                    .mention_token()
                    .map(|token| (CompletionKind::File, token))
            });
        let Some((kind, token)) = completion else {
            if matches!(self.input_overlay, Some(InputOverlay::Completion(_))) {
                self.dismiss_input_overlay();
            }
            return;
        };
        if kind == CompletionKind::Command && self.command_catalog.is_empty() {
            return;
        }
        let items = match kind {
            CompletionKind::Command => completion_items(&self.command_catalog, &token.query),
            CompletionKind::CommandValue => Vec::new(),
            CompletionKind::File => file_completion_items(&self.workspace_files, &token.query),
        };
        self.input_overlay = Some(InputOverlay::Completion(CompletionState {
            kind,
            token,
            items,
            selected: 0,
            parent_command: None,
        }));
    }

    fn move_completion(&mut self, delta: isize) {
        let Some(InputOverlay::Completion(state)) = &mut self.input_overlay else {
            return;
        };
        let len = state.items.len();
        if len == 0 {
            return;
        }
        state.selected = if delta < 0 {
            state.selected.checked_sub(1).unwrap_or(len - 1)
        } else {
            (state.selected + 1) % len
        };
    }

    fn open_command_values_for_draft(&mut self) -> bool {
        let text = self.editor.text();
        let Some((command, query)) = self.command_values.iter().find_map(|(command, values)| {
            let bare = format!("/{command}");
            if text == bare {
                return Some((command.clone(), String::new()));
            }
            let prefix = format!("{bare} ");
            let query = text.strip_prefix(&prefix)?;
            if query.contains(char::is_whitespace) || values.iter().any(|value| value.name == query)
            {
                return None;
            }
            Some((command.clone(), query.to_string()))
        }) else {
            return false;
        };
        self.open_command_values_with_query(&command, &query)
    }

    fn submit_editor(&mut self) -> AppCommand {
        if self.editor.text().trim_start().starts_with('/') {
            if self.editor.has_images() {
                let _ = self.push_runtime_item(
                    ItemKind::Notice,
                    "remove image attachments before running a slash command",
                );
                return AppCommand::None;
            }
            self.editor
                .submit_command()
                .map_or(AppCommand::None, AppCommand::RunSlash)
        } else {
            // Research is text-only: reject a research-routed prompt carrying images
            // BEFORE `submit()` consumes the draft, so the exact text/pastes/images
            // are preserved with no history/queue/worker side effect.
            if self.mode() == Mode::Research && self.editor.has_images() {
                let count = self.editor.image_count();
                let _ = self.push_runtime_item(
                    ItemKind::Notice,
                    format!(
                        "research is text-only — remove the {count} image(s) or /agent to exit research mode"
                    ),
                );
                return AppCommand::None;
            }
            self.editor
                .submit()
                .map_or(AppCommand::None, AppCommand::Submit)
        }
    }

    fn open_command_values(&mut self, command: &str) -> bool {
        self.open_command_values_with_query(command, "")
    }

    fn open_command_values_with_query(&mut self, command: &str, query: &str) -> bool {
        let Some(values) = self.command_values.get(command).cloned() else {
            return false;
        };
        if values.is_empty() {
            return false;
        }
        let prefix = format!("/{command} ");
        self.editor.replace_draft(format!("{prefix}{query}"));
        let start = prefix.len();
        let end = start.saturating_add(query.len());
        self.input_overlay = Some(InputOverlay::Completion(CompletionState {
            kind: CompletionKind::CommandValue,
            token: EditorToken {
                range: start..end,
                query: query.to_string(),
            },
            items: value_completion_items(&values, query),
            selected: 0,
            parent_command: Some(command.to_string()),
        }));
        true
    }

    fn refresh_command_value_completion(&mut self) {
        let Some(InputOverlay::Completion(state)) = &self.input_overlay else {
            return;
        };
        let Some(parent) = state.parent_command.clone() else {
            self.input_overlay = None;
            return;
        };
        let start = state.token.range.start;
        let prefix = format!("/{parent} ");
        let text = self.editor.text();
        let cursor = self.editor.cursor();
        if !text.starts_with(&prefix) || cursor < start {
            self.input_overlay = None;
            self.refresh_or_open_completion();
            return;
        }
        let before = &text[start..cursor];
        if before.contains(char::is_whitespace) {
            self.input_overlay = None;
            return;
        }
        let end = text[cursor..]
            .find(char::is_whitespace)
            .map_or(text.len(), |offset| cursor + offset);
        let query = before.to_string();
        let values = self
            .command_values
            .get(&parent)
            .map_or_else(Vec::new, |values| value_completion_items(values, &query));
        self.input_overlay = Some(InputOverlay::Completion(CompletionState {
            kind: CompletionKind::CommandValue,
            token: EditorToken {
                range: start..end,
                query,
            },
            items: values,
            selected: 0,
            parent_command: Some(parent),
        }));
    }

    fn accept_completion(&mut self) -> AppCommand {
        let Some(InputOverlay::Completion(state)) = &self.input_overlay else {
            return AppCommand::None;
        };
        let Some(item) = state.items.get(state.selected).cloned() else {
            return AppCommand::None;
        };
        let kind = state.kind;
        let range = state.token.range.clone();
        self.input_overlay = None;
        let replacement = match kind {
            CompletionKind::Command => format!("/{}", item.name),
            CompletionKind::CommandValue => item.name.clone(),
            CompletionKind::File => format!("@{} ", item.name),
        };
        self.editor.replace_range(range, &replacement);
        match kind {
            CompletionKind::Command if self.open_command_values(&item.name) => AppCommand::None,
            CompletionKind::CommandValue => self
                .editor
                .submit_command()
                .map_or(AppCommand::None, AppCommand::RunSlash),
            CompletionKind::Command | CompletionKind::File => AppCommand::None,
        }
    }

    fn reverse_history_index(&self) -> Option<usize> {
        let Some(InputOverlay::ReverseHistory(state)) = &self.input_overlay else {
            return None;
        };
        state.match_index
    }

    fn refresh_reverse_history(&mut self, before_exclusive: Option<usize>) {
        let Some(InputOverlay::ReverseHistory(state)) = &self.input_overlay else {
            return;
        };
        let query = state.query.clone();
        let original_draft = state.original_draft.clone();
        let found = self.editor.history_match_reverse(&query, before_exclusive);
        if let Some((index, text)) = found {
            self.editor.replace_draft(text);
            if let Some(InputOverlay::ReverseHistory(state)) = &mut self.input_overlay {
                state.match_index = Some(index);
            }
        } else if before_exclusive.is_none() {
            self.editor.restore_snapshot(original_draft);
            if let Some(InputOverlay::ReverseHistory(state)) = &mut self.input_overlay {
                state.match_index = None;
            }
        }
    }

    fn advance_reverse_history(&mut self) {
        let Some(InputOverlay::ReverseHistory(state)) = &self.input_overlay else {
            return;
        };
        let query = state.query.clone();
        let Some(index) = state.match_index else {
            self.refresh_reverse_history(None);
            return;
        };
        let Some((next_index, text)) = self.editor.history_match_forward(&query, index) else {
            return;
        };
        self.editor.replace_draft(text);
        if let Some(InputOverlay::ReverseHistory(state)) = &mut self.input_overlay {
            state.match_index = Some(next_index);
        }
    }

    fn append_active_to(
        projection: &mut SessionProjection,
        active: Option<ItemId>,
        text: &str,
    ) -> bool {
        active.is_some_and(|id| projection.timeline.append_text(id, text))
    }

    fn push_runtime_item(&mut self, kind: ItemKind, text: impl Into<String>) -> Option<ItemId> {
        Self::push_runtime_item_to(self.projections.active_mut(), kind, text)
    }

    fn push_runtime_item_to(
        projection: &mut SessionProjection,
        kind: ItemKind,
        text: impl Into<String>,
    ) -> Option<ItemId> {
        let text = text.into();
        if let Some(before) = projection.active_insert_before {
            projection.timeline.insert_before(before, kind, text)
        } else {
            projection.timeline.push(kind, text)
        }
    }

    fn style_transcript_on(projection: &mut SessionProjection) {
        for id in [projection.active_assistant, projection.active_reasoning]
            .into_iter()
            .flatten()
        {
            let Some(item) = projection.timeline.item(id) else {
                continue;
            };
            let styles = semantic_ranges(item.kind, &item.text);
            let _ = projection.timeline.set_styles(id, styles);
        }
    }

    fn style_activity_on(projection: &mut SessionProjection, id: ItemId, activity: ActivityState) {
        let Some((kind, text)) = projection
            .timeline
            .item(id)
            .map(|item| (item.kind, item.text.clone()))
        else {
            return;
        };
        let role = match (kind, activity) {
            (ItemKind::Question, ActivityState::Running | ActivityState::Success) => {
                SemanticRole::Tool
            }
            (ItemKind::Question, ActivityState::Error | ActivityState::Cancelled) => {
                SemanticRole::Muted
            }
            (_, ActivityState::Running) => SemanticRole::Tool,
            (_, ActivityState::Success) => SemanticRole::Success,
            (_, ActivityState::Error) => SemanticRole::Error,
            (_, ActivityState::Cancelled) => SemanticRole::Muted,
        };
        let styles = if kind == ItemKind::Tool {
            tool_activity_styles(&text, role)
        } else {
            (!text.is_empty())
                .then_some(StyledRange {
                    start_byte: 0,
                    end_byte: text.len(),
                    style: TextStyle::new(role),
                })
                .into_iter()
                .collect()
        };
        let _ = projection.timeline.set_styles(id, styles);
    }

    fn cancel_or_exit(&mut self) -> AppCommand {
        if self.exit_armed {
            self.exit_requested = true;
            return AppCommand::Exit;
        }

        if let Some(text) = self.trust_selected_text() {
            self.exit_armed = true;
            return AppCommand::Copy(text);
        }
        if let Some(text) = self.projections.active().timeline.selected_text() {
            self.exit_armed = true;
            return AppCommand::Copy(text);
        }

        // Preserve a typed prompt (including paste/image attachments) in the
        // existing atomic stash before Ctrl+C escalates to cancelling work or
        // exiting the process. Clearing a draft is not an exit-arm press.
        if !self.editor.text().is_empty() {
            self.stash_or_pop();
            self.exit_armed = false;
            return AppCommand::None;
        }

        self.exit_armed = true;
        match self.projections.active().work {
            WorkState::Idle => AppCommand::None,
            WorkState::Busy {
                cancellation_requested: false,
            } => {
                self.projections.active_mut().work = WorkState::Busy {
                    cancellation_requested: true,
                };
                AppCommand::CancelWork
            }
            WorkState::Busy {
                cancellation_requested: true,
            } => AppCommand::None,
        }
    }

    fn stash_or_pop(&mut self) {
        if self.editor.text().is_empty() {
            if let Some(snapshot) = self.stashed_draft.take() {
                self.editor.restore_snapshot(snapshot);
            }
        } else {
            self.stashed_draft = Some(self.editor.snapshot());
            self.editor.clear_all();
        }
    }

    fn escape_or_interrupt(&mut self, now: Instant) -> AppCommand {
        if !matches!(self.projections.active().work, WorkState::Idle) {
            self.escape_armed_at = None;
            return self.interrupt_work();
        }
        if self.focus != Focus::Composer || self.editor.text().is_empty() {
            self.escape_armed_at = None;
            return AppCommand::None;
        }
        let clears_draft = self.escape_armed_at.is_some_and(|armed| {
            now.checked_duration_since(armed)
                .is_some_and(|elapsed| elapsed <= DOUBLE_ESCAPE_WINDOW)
        });
        if clears_draft {
            self.escape_armed_at = None;
            self.editor.clear_all();
        } else {
            self.escape_armed_at = Some(now);
        }
        AppCommand::None
    }

    fn interrupt_work(&mut self) -> AppCommand {
        match self.projections.active().work {
            WorkState::Busy {
                cancellation_requested: false,
            } => {
                self.projections.active_mut().work = WorkState::Busy {
                    cancellation_requested: true,
                };
                AppCommand::CancelWork
            }
            WorkState::Idle
            | WorkState::Busy {
                cancellation_requested: true,
            } => AppCommand::None,
        }
    }
}

fn completion_items(catalog: &[CompletionCommand], query: &str) -> Vec<CompletionItem> {
    let mut matches = catalog
        .iter()
        .enumerate()
        .filter_map(|(order, command)| {
            fuzzy_score(&command.name, query).map(|score| (score, order, command))
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|(score, order, _)| (*score, *order));
    matches
        .into_iter()
        .map(|(_, _, command)| CompletionItem {
            name: command.name.clone(),
            description: command.description.clone(),
        })
        .collect()
}

fn value_completion_items(catalog: &[CompletionItem], query: &str) -> Vec<CompletionItem> {
    let mut matches = catalog
        .iter()
        .enumerate()
        .filter_map(|(order, value)| {
            fuzzy_score(&value.name, query).map(|score| (score, order, value))
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|(score, order, _)| (*score, *order));
    matches
        .into_iter()
        .map(|(_, _, value)| value.clone())
        .collect()
}

fn timeline_search_matches(timeline: &Timeline, query: &str) -> Vec<ContentPoint> {
    if query.is_empty() {
        return Vec::new();
    }
    timeline
        .items()
        .iter()
        // A hidden item has no visible row, so it must not produce a search match
        // (a match the UI could not scroll to). Same authority as render/layout.
        .filter(|item| timeline.item_is_visible(item))
        .filter_map(|item| {
            case_insensitive_match_byte(&item.text, query).map(|byte| ContentPoint {
                item_id: item.id,
                byte,
            })
        })
        .collect()
}

fn case_insensitive_match_byte(text: &str, query: &str) -> Option<usize> {
    let folded_query = query
        .chars()
        .flat_map(char::to_lowercase)
        .collect::<String>();
    if folded_query.is_empty() {
        return None;
    }
    let mut folded_text = String::new();
    let mut original_bytes = Vec::new();
    for (original_byte, character) in text.char_indices() {
        for folded_character in character.to_lowercase() {
            let before = folded_text.len();
            folded_text.push(folded_character);
            original_bytes.extend(std::iter::repeat_n(
                original_byte,
                folded_text.len().saturating_sub(before),
            ));
        }
    }
    let folded_byte = folded_text.find(&folded_query)?;
    original_bytes.get(folded_byte).copied()
}

fn file_completion_items(files: &[String], query: &str) -> Vec<CompletionItem> {
    let mut matches = files
        .iter()
        .enumerate()
        .filter_map(|(order, path)| {
            let basename = path.rsplit('/').next().unwrap_or(path);
            fuzzy_score(basename, query)
                .or_else(|| fuzzy_score(path, query).map(|score| score.saturating_add(20_000)))
                .map(|score| (score, order, path))
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|(score, order, _)| (*score, *order));
    matches
        .into_iter()
        .take(50)
        .map(|(_, _, path)| CompletionItem {
            name: path.clone(),
            description: "workspace file".to_string(),
        })
        .collect()
}

fn fuzzy_score(candidate: &str, query: &str) -> Option<usize> {
    if query.is_empty() {
        return Some(0);
    }
    let candidate = candidate.to_lowercase();
    let query = query.to_lowercase();
    let candidate_chars = candidate.chars().collect::<Vec<_>>();
    let mut search_from = 0;
    let mut first = None;
    let mut previous = None;
    let mut gaps = 0usize;
    for needle in query.chars() {
        let relative = candidate_chars[search_from..]
            .iter()
            .position(|character| *character == needle)?;
        let found = search_from + relative;
        first.get_or_insert(found);
        if let Some(previous) = previous {
            gaps = gaps.saturating_add(found.saturating_sub(previous + 1));
        }
        previous = Some(found);
        search_from = found + 1;
    }
    let prefix_penalty = usize::from(!candidate.starts_with(&query)).saturating_mul(10_000);
    Some(
        prefix_penalty
            .saturating_add(first.unwrap_or_default().saturating_mul(100))
            .saturating_add(gaps),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ViewportAnchor;

    fn model() -> AppModel {
        AppModel::new(
            Header {
                version: "0".to_string(),
                provider: "test".to_string(),
                model: "test-model".to_string(),
                workspace: "workspace".to_string(),
                branch: Some("main".to_string()),
                workspace_dirty: Some(false),
                mode: Mode::Agent,
                profile: "default".to_string(),
                session_id: "session".to_string(),
                session_name: None,
            },
            TerminalCapabilities::default(),
        )
    }

    fn pair_model() -> AppModel {
        AppModel::new_pair(
            Header {
                version: "0".to_string(),
                provider: "provider-a".to_string(),
                model: "model-a".to_string(),
                workspace: "workspace".to_string(),
                branch: Some("main".to_string()),
                workspace_dirty: Some(false),
                mode: Mode::Agent,
                profile: "default".to_string(),
                session_id: "session-a".to_string(),
                session_name: Some("Alpha".to_string()),
            },
            SessionHeader {
                provider: "provider-b".to_string(),
                model: "model-b".to_string(),
                session_id: "session-b".to_string(),
                session_name: Some("Beta".to_string()),
            },
            TerminalCapabilities::default(),
        )
    }

    #[test]
    fn explicit_accessors_project_the_existing_single_state_without_transformation() {
        let mut app = model();
        assert!(app.pair_status().is_none());
        assert!(!app.set_pair_status(PairStatus {
            completed_rounds: 0,
            max_rounds: 3,
            scheduled: Some(PeerPane::A),
            candidate: None,
            agreements: [false, false],
            repairing: None,
            terminal: None,
        }));
        assert_eq!(app.shared_version(), "0");
        assert_eq!(app.shared_workspace(), "workspace");
        assert_eq!(app.shared_branch(), Some("main"));
        assert_eq!(app.shared_workspace_dirty(), Some(false));
        assert_eq!(app.shared_mode(), "agent");
        assert_eq!(app.shared_profile(), "default");

        app.set_active_provider_model("provider\nraw".to_string(), "model\traw".to_string());
        app.set_active_session_id("session/raw".to_string());
        app.set_active_session_name(Some("name\nraw".to_string()));
        assert_eq!(app.active_provider(), "provider\nraw");
        assert_eq!(app.active_model(), "model\traw");
        assert_eq!(app.active_session_id(), "session/raw");
        assert_eq!(app.active_session_name(), Some("name\nraw"));

        let item = app
            .active_timeline_mut()
            .push(ItemKind::Notice, "existing projection")
            .expect("timeline item");
        assert!(app.active_timeline().item(item).is_some());
        app.begin_work();
        assert_eq!(
            app.active_work(),
            WorkState::Busy {
                cancellation_requested: false
            }
        );
        app.apply_runtime(RuntimeUpdate::Plan(vec![PlanEntry {
            title: "step".to_string(),
            status: "pending".to_string(),
        }]));
        assert_eq!(app.active_plan()[0].title, "step");
        app.set_active_usage(Some(UsageTotals {
            input_tokens: 3,
            output_tokens: 2,
            cached_input_tokens: 1,
        }));
        app.set_active_context_usage(Some((5, 8)));
        assert_eq!(app.active_usage().expect("usage").total(), 5);
        assert_eq!(app.active_context_usage(), Some((5, 8)));
        app.apply_runtime(RuntimeUpdate::Text("bytes".to_string()));
        assert_eq!(app.active_stream_bytes(), 5);
    }

    #[test]
    fn pair_status_is_sanitized_and_never_changes_the_active_projection() {
        let mut app = pair_model();
        assert!(app.begin_work_for(PeerPane::B));
        assert_eq!(app.active_pair_pane(), Some(PeerPane::A));
        assert_eq!(
            app.projection(PeerPane::B)
                .map(|projection| projection.work),
            Some(WorkState::Busy {
                cancellation_requested: false,
            })
        );
        assert!(app.set_pair_status(PairStatus {
            completed_rounds: 2,
            max_rounds: 3,
            scheduled: Some(PeerPane::B),
            candidate: Some(PairStatusCandidate {
                revision: 4,
                full_digest: "ab\ncd".to_string(),
            }),
            agreements: [true, false],
            repairing: Some(PeerPane::B),
            terminal: Some("converged\nraw".to_string()),
        }));

        assert_eq!(
            app.pair_status(),
            Some(&PairStatus {
                completed_rounds: 2,
                max_rounds: 3,
                scheduled: Some(PeerPane::B),
                candidate: Some(PairStatusCandidate {
                    revision: 4,
                    full_digest: "ab cd".to_string(),
                }),
                agreements: [true, false],
                repairing: Some(PeerPane::B),
                terminal: Some("converged raw".to_string()),
            })
        );
        assert_eq!(app.active_pair_pane(), Some(PeerPane::A));
    }

    #[test]
    fn named_prompt_activation_is_peer_local_and_focus_neutral() {
        let mut app = pair_model();
        let a = app
            .append_prompt("alpha steering", None, true)
            .expect("A prompt");
        assert!(app.select_pair_pane(PeerPane::B));
        let b = app
            .append_prompt("beta steering", None, true)
            .expect("B prompt");

        assert!(app.activate_prompt_for(PeerPane::A, a));
        assert_eq!(app.active_pair_pane(), Some(PeerPane::B));
        assert!(
            !app.timeline_for(PeerPane::A)
                .expect("A timeline")
                .item(a)
                .expect("A prompt remains")
                .pending
        );
        assert!(
            app.timeline_for(PeerPane::B)
                .expect("B timeline")
                .item(b)
                .expect("B prompt remains")
                .pending
        );
        assert!(!model().activate_prompt_for(PeerPane::A, a));
    }

    #[test]
    fn append_result_for_is_pair_only_sanitized_and_selectable() {
        use crate::ContentPoint;

        // Single chat rejects a result and leaves its timeline byte-for-byte unchanged.
        let mut single = model();
        let before = single.active_timeline().items().to_vec();
        assert!(!single.append_result_for(PeerPane::A, "x".to_string(), ResultTone::Error));
        assert_eq!(
            single.active_timeline().items().to_vec(),
            before,
            "a rejected result must not mutate any item content, kind, style, or tone"
        );

        // Pair mode appends a sanitized, toned result into the named projection only.
        let mut pair = pair_model();
        assert!(pair.append_result_for(
            PeerPane::A,
            "kept body\u{7}tail".to_string(),
            ResultTone::Incomplete
        ));
        assert!(pair
            .timeline_for(PeerPane::B)
            .expect("B timeline")
            .items()
            .iter()
            .all(|item| item.kind != ItemKind::Result));

        let (id, tone, sanitized) = {
            let item = pair
                .timeline_for(PeerPane::A)
                .expect("A timeline")
                .items()
                .iter()
                .find(|item| item.kind == ItemKind::Result)
                .expect("a result row");
            (item.id, item.tone, item.text.clone())
        };
        assert_eq!(tone, Some(ResultTone::Incomplete));
        assert!(!sanitized.contains('\u{7}'), "control text is sanitized");

        // The result row is a generic selectable timeline item: selecting it and
        // reading the selection returns its copied text.
        let timeline = pair.timeline_for_mut(PeerPane::A).expect("A timeline");
        timeline.start_selection(ContentPoint {
            item_id: id,
            byte: 0,
        });
        timeline.extend_selection(ContentPoint {
            item_id: id,
            byte: sanitized.len(),
        });
        assert_eq!(
            timeline.selected_text().as_deref(),
            Some(sanitized.as_str())
        );
    }

    #[test]
    fn request_pair_cancellation_marks_busy_panes_and_leaves_others_and_single_chat() {
        // Single chat mutates nothing and reports false.
        let mut single = model();
        assert!(!single.request_pair_cancellation());

        // A busy, B idle. Requesting cancellation flips only A, and never faces focus.
        let mut pair = pair_model();
        assert!(pair.begin_work_for(PeerPane::A));
        let focus = pair.active_pair_pane();
        assert!(pair.request_pair_cancellation());
        assert_eq!(pair.active_pair_pane(), focus, "focus is unchanged");
        assert_eq!(
            pair.projections.projection(PeerPane::A).expect("A").work,
            WorkState::Busy {
                cancellation_requested: true
            }
        );
        assert_eq!(
            pair.projections.projection(PeerPane::B).expect("B").work,
            WorkState::Idle
        );

        // Idempotent: a second request leaves the flagged pane exactly as it was.
        assert!(pair.request_pair_cancellation());
        assert_eq!(
            pair.projections.projection(PeerPane::A).expect("A").work,
            WorkState::Busy {
                cancellation_requested: true
            }
        );

        // With BOTH panes busy, a request marks both — and still never moves focus.
        assert!(pair.begin_work_for(PeerPane::B));
        let focus = pair.active_pair_pane();
        assert!(pair.request_pair_cancellation());
        assert_eq!(pair.active_pair_pane(), focus, "focus is unchanged");
        for pane in [PeerPane::A, PeerPane::B] {
            assert_eq!(
                pair.projections.projection(pane).expect("pane").work,
                WorkState::Busy {
                    cancellation_requested: true
                },
                "pane {pane:?} is marked cancelling",
            );
        }
    }

    #[test]
    fn peer_switching_preserves_shared_surfaces_and_resumes_named_searches() {
        let mut app = pair_model();
        app.active_timeline_mut()
            .push(ItemKind::User, "alpha marker")
            .expect("A item");
        app.open_timeline_search("alpha".to_string());
        assert!(app.select_pair_pane(PeerPane::B));
        app.active_timeline_mut()
            .push(ItemKind::User, "beta marker")
            .expect("B item");
        app.open_timeline_search("beta".to_string());

        assert!(app.select_pair_pane(PeerPane::A));
        assert_eq!(app.timeline_search().expect("A search").query, "alpha");
        assert_eq!(
            app.handle_input(InputAction::CyclePeer, 80),
            AppCommand::None
        );
        assert_eq!(app.active_pair_pane(), Some(PeerPane::B));
        assert_eq!(app.timeline_search().expect("B search").query, "beta");
        assert_eq!(
            app.handle_input(InputAction::CyclePeer, 80),
            AppCommand::None
        );
        assert_eq!(
            app.timeline_search().expect("A search again").query,
            "alpha"
        );

        app.projections.active_mut().timeline_search = None;
        app.editor.replace_draft("/mo");
        app.set_command_catalog([CompletionCommand {
            name: "model".to_string(),
            description: "switch model".to_string(),
        }]);
        app.refresh_or_open_completion();
        app.quick_help = true;
        assert!(app.completion().is_some());
        assert_eq!(
            app.handle_input(InputAction::CyclePeer, 80),
            AppCommand::None
        );
        assert_eq!(app.active_pair_pane(), Some(PeerPane::B));
        assert!(app.completion().is_some());
        assert!(app.quick_help());

        let mut takeover = pair_model();
        takeover.open_help();
        assert_eq!(
            takeover.handle_input(InputAction::CyclePeer, 80),
            AppCommand::None
        );
        assert_eq!(takeover.active_pair_pane(), Some(PeerPane::B));
        assert!(takeover.has_takeover());

        let mut theme = pair_model();
        theme.open_theme_picker();
        assert_eq!(
            theme.handle_input(InputAction::CyclePeer, 80),
            AppCommand::None
        );
        assert_eq!(theme.active_pair_pane(), Some(PeerPane::B));
        assert!(theme.has_theme_picker());

        let mut single = model();
        assert!(!single.select_pair_pane(PeerPane::A));
        assert!(!single.cycle_pair_pane());
    }

    #[test]
    fn named_dialogs_lock_and_retain_the_requesting_peer() {
        let mut app = pair_model();
        assert!(app.request_approval_for(PeerPane::B, "write", "target", "ask"));
        assert_eq!(app.active_pair_pane(), Some(PeerPane::B));
        assert_eq!(app.dialog_peer(), Some(PeerPane::B));
        assert!(!app.select_pair_pane(PeerPane::A));
        assert!(!app.cycle_pair_pane());
        assert!(!app.request_question_for(
            PeerPane::A,
            None,
            "replace?",
            Vec::<QuestionOption>::new(),
            false,
            1,
            1,
        ));
        assert_eq!(app.dialog_peer(), Some(PeerPane::B));

        app.clear_dialog();
        assert_eq!(app.dialog_peer(), None);
        assert_eq!(app.active_pair_pane(), Some(PeerPane::B));
        assert!(app.cycle_pair_pane());
        app.request_question(
            None,
            "continue?",
            [QuestionOption {
                label: "Yes".to_string(),
                description: None,
            }],
            false,
            1,
            1,
        );
        assert_eq!(app.dialog_peer(), Some(PeerPane::A));

        let mut single = model();
        assert!(!single.request_approval_for(PeerPane::A, "write", "target", "ask"));
        assert_eq!(single.dialog, None);
        assert_eq!(single.dialog_peer(), None);
    }

    #[test]
    fn paired_session_projections_isolate_live_state_and_retain_searches() {
        let mut app = AppModel::new_pair(
            Header {
                version: "0".to_string(),
                provider: "provider-a".to_string(),
                model: "model-a".to_string(),
                workspace: "workspace".to_string(),
                branch: Some("main".to_string()),
                workspace_dirty: Some(false),
                mode: Mode::Agent,
                profile: "default".to_string(),
                session_id: "session-a".to_string(),
                session_name: Some("Alpha".to_string()),
            },
            SessionHeader {
                provider: "provider-b\x1b[2J".to_string(),
                model: "model-b\rnext".to_string(),
                session_id: "session-b\nnext".to_string(),
                session_name: Some("Beta\tName".to_string()),
            },
            TerminalCapabilities::default(),
        );
        assert!(app.is_pair());
        assert_eq!(app.active_pair_pane(), Some(PeerPane::A));
        assert_eq!(app.shared_workspace(), "workspace");

        app.editor.insert("draft-a");
        let a_insert = app
            .active_timeline_mut()
            .push(ItemKind::User, "queued alpha")
            .expect("A insertion point");
        app.begin_work_before(Some(a_insert));
        app.apply_runtime(RuntimeUpdate::Text("alpha response".to_string()));
        app.apply_runtime(RuntimeUpdate::Reasoning("alpha reasoning".to_string()));
        app.apply_runtime(RuntimeUpdate::ToolStarted {
            id: "tool-a".to_string(),
            name: "inspect".to_string(),
            detail: "alpha detail".to_string(),
        });
        app.apply_runtime(RuntimeUpdate::Plan(vec![PlanEntry {
            title: "alpha step".to_string(),
            status: "pending".to_string(),
        }]));
        app.apply_runtime(RuntimeUpdate::Usage {
            input_tokens: 3,
            output_tokens: 2,
            cached_input_tokens: 1,
        });
        app.apply_runtime(RuntimeUpdate::ContextUsage { used: 5, limit: 8 });
        let selected = app.active_timeline().items()[0].id;
        app.active_timeline_mut().start_selection(ContentPoint {
            item_id: selected,
            byte: 0,
        });
        app.active_timeline_mut().extend_selection(ContentPoint {
            item_id: selected,
            byte: 5,
        });
        app.open_timeline_search("alpha".to_string());
        let a_viewport = app.active_timeline().viewport;

        let b = app
            .projections
            .projection(PeerPane::B)
            .expect("peer B projection");
        assert!(b.timeline.items().is_empty());
        assert_eq!(b.work, WorkState::Idle);
        assert!(b.plan.is_empty());
        assert_eq!(b.usage, None);
        assert_eq!(b.context_usage, None);
        assert_eq!(b.stream_bytes, 0);
        assert!(b.timeline_search.is_none());
        assert!(b.active_assistant.is_none());
        assert!(b.active_reasoning.is_none());
        assert!(b.active_tools.is_empty());
        assert!(b.active_insert_before.is_none());
        assert!(b.timeline.selection.is_none());

        app.projections.select(PeerPane::B);
        assert_eq!(app.active_pair_pane(), Some(PeerPane::B));
        assert_eq!(app.active_provider(), "provider-b");
        assert_eq!(app.active_model(), "model-b\nnext");
        assert_eq!(app.active_session_id(), "session-b next");
        assert_eq!(app.active_session_name(), Some("Beta Name"));
        app.set_active_provider_model("provider-b-raw\n".to_string(), "model-b\t".to_string());
        app.set_active_session_name(Some("Beta raw\n".to_string()));
        app.editor.insert("draft-b");
        app.begin_work();
        app.apply_runtime(RuntimeUpdate::Text("beta response".to_string()));
        app.apply_runtime(RuntimeUpdate::Reasoning("beta reasoning".to_string()));
        app.apply_runtime(RuntimeUpdate::ToolStarted {
            id: "tool-b".to_string(),
            name: "inspect".to_string(),
            detail: "beta detail".to_string(),
        });
        app.apply_runtime(RuntimeUpdate::Plan(vec![PlanEntry {
            title: "beta step".to_string(),
            status: "active".to_string(),
        }]));
        app.apply_runtime(RuntimeUpdate::Usage {
            input_tokens: 11,
            output_tokens: 7,
            cached_input_tokens: 4,
        });
        app.apply_runtime(RuntimeUpdate::ContextUsage {
            used: 13,
            limit: 21,
        });
        app.open_timeline_search("beta".to_string());

        let a = app
            .projections
            .projection(PeerPane::A)
            .expect("peer A projection");
        assert_eq!(a.header.provider, "provider-a");
        assert_eq!(a.header.model, "model-a");
        assert_eq!(a.header.session_id, "session-a");
        assert_eq!(a.header.session_name.as_deref(), Some("Alpha"));
        assert_eq!(
            a.work,
            WorkState::Busy {
                cancellation_requested: false
            }
        );
        assert_eq!(a.plan[0].title, "alpha step");
        assert_eq!(a.usage.expect("A usage").total(), 5);
        assert_eq!(a.context_usage, Some((5, 8)));
        assert_eq!(a.stream_bytes, "alpha responsealpha reasoning".len());
        // The tool boundary retires the open assistant/reasoning segments; the
        // items themselves remain (asserted via timeline/stream_bytes below).
        assert!(a.active_assistant.is_none());
        assert!(a.active_reasoning.is_none());
        assert!(a.active_tools.contains_key("tool-a"));
        assert_eq!(a.active_insert_before, Some(a_insert));
        assert_eq!(a.timeline.selected_text().as_deref(), Some("alpha"));
        assert_eq!(a.timeline.viewport, a_viewport);
        assert_eq!(
            a.timeline_search.as_ref().map(|state| state.query.as_str()),
            Some("alpha")
        );

        let b = app
            .projections
            .projection(PeerPane::B)
            .expect("peer B projection");
        assert_eq!(b.header.provider, "provider-b-raw\n");
        assert_eq!(b.header.model, "model-b\t");
        assert_eq!(b.header.session_id, "session-b next");
        assert_eq!(b.header.session_name.as_deref(), Some("Beta raw\n"));
        assert_eq!(
            b.work,
            WorkState::Busy {
                cancellation_requested: false
            }
        );
        assert_eq!(b.plan[0].title, "beta step");
        assert_eq!(b.usage.expect("B usage").total(), 18);
        assert_eq!(b.context_usage, Some((13, 21)));
        assert_eq!(b.stream_bytes, "beta responsebeta reasoning".len());
        // The tool boundary retires the open assistant/reasoning segments; the
        // items themselves remain (asserted via timeline/stream_bytes below).
        assert!(b.active_assistant.is_none());
        assert!(b.active_reasoning.is_none());
        assert!(b.active_tools.contains_key("tool-b"));
        assert!(b.timeline.selection.is_none());
        assert_eq!(
            b.timeline_search.as_ref().map(|state| state.query.as_str()),
            Some("beta")
        );

        app.projections.select(PeerPane::A);
        app.cancel_input_overlay();
        assert_eq!(app.editor.text(), "draft-a");
        assert!(app.timeline_search().is_none());
        app.editor.clear_all();
        app.projections.select(PeerPane::B);
        app.cancel_input_overlay();
        assert_eq!(app.editor.text(), "draft-b");
        assert!(app.timeline_search().is_none());
        assert!(app.select_pair_pane(PeerPane::A));
        app.request_approval("shared tool", "shared target", "ask");
        assert!(matches!(app.dialog, Some(DialogState::Approval { .. })));
        assert_eq!(app.dialog_peer(), Some(PeerPane::A));
        assert!(!app.select_pair_pane(PeerPane::B));
        assert_eq!(app.active_pair_pane(), Some(PeerPane::A));
        assert!(matches!(app.dialog, Some(DialogState::Approval { .. })));

        app.clear_conversation();
        let a = app
            .projections
            .projection(PeerPane::A)
            .expect("peer A projection");
        assert!(a.timeline.items().is_empty());
        assert_eq!(a.work, WorkState::Idle);
        assert!(a.plan.is_empty());
        assert_eq!(a.usage, None);
        assert_eq!(a.context_usage, None);
        assert_eq!(a.stream_bytes, 0);
        let b = app
            .projections
            .projection(PeerPane::B)
            .expect("peer B projection");
        assert!(b
            .timeline
            .items()
            .iter()
            .any(|item| item.text == "beta response"));
        assert_eq!(b.plan[0].title, "beta step");
        assert_eq!(b.usage.expect("B usage remains").total(), 18);
        assert!(matches!(app.dialog, Some(DialogState::Approval { .. })));
    }

    #[test]
    fn named_runtime_updates_leave_the_active_peer_unchanged() {
        let mut app = AppModel::new_pair(
            Header {
                version: "0".to_string(),
                provider: "provider-a".to_string(),
                model: "model-a".to_string(),
                workspace: "workspace".to_string(),
                branch: Some("main".to_string()),
                workspace_dirty: Some(false),
                mode: Mode::Agent,
                profile: "default".to_string(),
                session_id: "session-a".to_string(),
                session_name: Some("Alpha".to_string()),
            },
            SessionHeader {
                provider: "provider-b".to_string(),
                model: "model-b".to_string(),
                session_id: "session-b".to_string(),
                session_name: Some("Beta".to_string()),
            },
            TerminalCapabilities::default(),
        );
        app.apply_runtime(RuntimeUpdate::Text("alpha response".to_string()));
        app.projections.select(PeerPane::B);
        app.open_timeline_search("beta".to_string());
        app.projections.select(PeerPane::A);

        assert!(app.apply_runtime_for(PeerPane::B, RuntimeUpdate::Text("beta response".into())));
        assert!(app.apply_runtime_for(
            PeerPane::B,
            RuntimeUpdate::Reasoning("beta reasoning".into())
        ));
        assert!(app.apply_runtime_for(
            PeerPane::B,
            RuntimeUpdate::ToolStarted {
                id: "tool-b".into(),
                name: "inspect".into(),
                detail: "beta detail".into(),
            }
        ));
        assert!(app.apply_runtime_for(
            PeerPane::B,
            RuntimeUpdate::Plan(vec![PlanEntry {
                title: "beta step".into(),
                status: "active".into(),
            }])
        ));
        assert!(app.apply_runtime_for(
            PeerPane::B,
            RuntimeUpdate::Usage {
                input_tokens: 8,
                output_tokens: 5,
                cached_input_tokens: 3,
            }
        ));
        assert!(app.apply_runtime_for(
            PeerPane::B,
            RuntimeUpdate::ContextUsage {
                used: 13,
                limit: 21,
            }
        ));

        assert_eq!(app.active_pair_pane(), Some(PeerPane::A));
        assert_eq!(app.active_stream_bytes(), "alpha response".len());
        assert_eq!(app.active_timeline().items().len(), 1);
        assert_eq!(app.active_timeline().items()[0].text, "alpha response");
        let b = app.projection(PeerPane::B).expect("peer B projection");
        assert_eq!(b.stream_bytes, "beta responsebeta reasoning".len());
        assert_eq!(b.plan[0].title, "beta step");
        assert_eq!(b.usage.expect("B usage").total(), 13);
        assert_eq!(b.context_usage, Some((13, 21)));
        // The tool boundary retires the open assistant/reasoning segments; the
        // items and stream_bytes above still prove B received the updates.
        assert!(b.active_assistant.is_none());
        assert!(b.active_reasoning.is_none());
        assert!(b.active_tools.contains_key("tool-b"));
        assert!(b
            .timeline_search
            .as_ref()
            .is_some_and(|search| !search.matches.is_empty()));

        let mut single = model();
        assert!(
            !single.apply_runtime_for(PeerPane::A, RuntimeUpdate::Text("must not appear".into()))
        );
        assert!(single.active_timeline().items().is_empty());
        assert_eq!(single.active_stream_bytes(), 0);
    }

    #[test]
    fn usage_updates_accumulate_for_the_whole_session() {
        let mut app = model();
        app.apply_runtime(RuntimeUpdate::Usage {
            input_tokens: 100,
            output_tokens: 20,
            cached_input_tokens: 60,
        });
        app.apply_runtime(RuntimeUpdate::Usage {
            input_tokens: 7,
            output_tokens: 3,
            cached_input_tokens: 5,
        });
        assert_eq!(
            app.active_usage(),
            Some(UsageTotals {
                input_tokens: 107,
                output_tokens: 23,
                cached_input_tokens: 65,
            })
        );
    }

    fn command(name: &str, description: &str) -> CompletionCommand {
        CompletionCommand {
            name: name.to_string(),
            description: description.to_string(),
        }
    }

    #[test]
    fn ctrl_c_stashes_an_idle_draft_without_arming_exit() {
        let mut app = model();
        app.editor.insert("unfinished draft");
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::None
        );
        assert!(app.editor.text().is_empty());
        assert!(app.has_stashed_draft());
        assert!(!app.exit_armed);
        assert!(!app.exit_requested);

        let _ = app.handle_input(InputAction::StashOrPop, 80);
        assert_eq!(app.editor.text(), "unfinished draft");
        assert!(!app.has_stashed_draft());
    }

    #[test]
    fn ctrl_c_idle_draft_clear_then_arm_then_exit() {
        let mut app = model();
        app.editor.insert("unfinished draft");
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::None
        );
        assert!(!app.exit_armed);
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::None
        );
        assert!(app.exit_armed);
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::Exit
        );
        assert!(app.exit_requested);
    }

    #[test]
    fn ctrl_c_busy_draft_clear_then_cancel_then_exit() {
        let mut app = model();
        app.begin_work();
        app.editor.insert("next prompt");

        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::None
        );
        assert!(app.editor.text().is_empty());
        assert!(app.has_stashed_draft());
        assert!(!app.exit_armed);
        assert_eq!(
            app.active_work(),
            WorkState::Busy {
                cancellation_requested: false
            }
        );

        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::CancelWork
        );
        assert!(app.exit_armed);
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::Exit
        );
    }

    #[test]
    fn ctrl_c_cancels_then_exits_while_cancellation_is_pending() {
        let mut app = model();
        app.begin_work();
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::CancelWork
        );
        assert_eq!(
            app.active_work(),
            WorkState::Busy {
                cancellation_requested: true
            }
        );
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::Exit
        );
    }

    #[test]
    fn ctrl_c_copies_a_selection_then_a_second_press_exits() {
        let mut app = model();
        app.editor.insert("draft stays while copying");
        let id = app
            .active_timeline_mut()
            .push(ItemKind::Assistant, "copy this")
            .expect("timeline id");
        app.active_timeline_mut()
            .start_selection(crate::ContentPoint {
                item_id: id,
                byte: 0,
            });
        app.active_timeline_mut()
            .extend_selection(crate::ContentPoint {
                item_id: id,
                byte: 4,
            });

        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::Copy("copy".to_string())
        );
        assert_eq!(app.editor.text(), "draft stays while copying");
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::Exit
        );
    }

    #[test]
    fn other_input_disarms_a_pending_exit() {
        let mut app = model();
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::None
        );
        assert_eq!(
            app.handle_input(InputAction::Insert("x".to_string()), 80),
            AppCommand::None
        );
        assert!(!app.exit_armed);
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::None
        );
    }

    #[test]
    fn timeline_navigation_is_semantic_and_disarms_exit() {
        let mut app = model();
        app.exit_armed = true;
        assert_eq!(
            app.handle_input(
                InputAction::NavigateTimeline(TimelineNavigation::PageUp),
                80,
            ),
            AppCommand::NavigateTimeline(TimelineNavigation::PageUp)
        );
        assert!(!app.exit_armed);
    }

    #[test]
    fn streamed_text_keeps_one_stable_timeline_id() {
        let mut app = model();
        app.begin_work();
        app.apply_runtime(RuntimeUpdate::Text("hello ".to_string()));
        let id = app.active_timeline().items()[0].id;
        app.apply_runtime(RuntimeUpdate::Text("world".to_string()));
        assert_eq!(app.active_timeline().items().len(), 1);
        assert_eq!(app.active_timeline().items()[0].id, id);
        assert_eq!(app.active_timeline().items()[0].text, "hello world");
        assert_eq!(app.active_stream_bytes(), "hello world".len());
    }

    #[test]
    fn new_stream_segments_strip_only_leading_framing_newlines() {
        let mut app = model();
        app.begin_work();
        let assistant = "\r\n\nanswer";
        let reasoning = "\n\rthinking";

        app.apply_runtime(RuntimeUpdate::Text(assistant.to_string()));
        app.apply_runtime(RuntimeUpdate::Reasoning(reasoning.to_string()));

        let items = app.active_timeline().items();
        assert_eq!(items[0].text, "answer");
        assert_eq!(items[1].text, "thinking");
        assert_eq!(app.active_stream_bytes(), assistant.len() + reasoning.len());
    }

    #[test]
    fn whitespace_only_segment_openers_are_dropped_but_bytes_are_counted() {
        let mut app = model();
        app.begin_work();

        app.apply_runtime(RuntimeUpdate::Text("\r\n\n".to_string()));
        app.apply_runtime(RuntimeUpdate::Reasoning("\n  \t".to_string()));

        assert!(app.active_timeline().items().is_empty());
        assert_eq!(app.active_stream_bytes(), 7);

        app.apply_runtime(RuntimeUpdate::Text("answer".to_string()));
        assert_eq!(app.active_timeline().items()[0].text, "answer");
    }

    #[test]
    fn mid_segment_leading_newlines_are_preserved() {
        let mut app = model();
        app.begin_work();

        app.apply_runtime(RuntimeUpdate::Text("first".to_string()));
        app.apply_runtime(RuntimeUpdate::Text("\nsecond\n".to_string()));
        app.apply_runtime(RuntimeUpdate::Reasoning("thinking".to_string()));
        app.apply_runtime(RuntimeUpdate::Reasoning("\nmore".to_string()));

        let items = app.active_timeline().items();
        assert_eq!(items[0].text, "first\nsecond\n");
        assert_eq!(items[1].text, "thinking\nmore");
    }

    #[test]
    fn escape_interrupts_work_without_arming_process_exit() {
        let mut app = model();
        assert_eq!(app.handle_input(InputAction::Escape, 80), AppCommand::None);
        app.begin_work();
        assert_eq!(
            app.handle_input(InputAction::Escape, 80),
            AppCommand::CancelWork
        );
        assert!(!app.exit_armed);
        assert_eq!(
            app.active_work(),
            WorkState::Busy {
                cancellation_requested: true
            }
        );
    }

    #[test]
    fn double_escape_clears_an_idle_draft_only_inside_its_own_time_window() {
        let start = Instant::now();
        let mut app = model();
        app.editor.insert("unfinished draft");

        assert_eq!(
            app.handle_input_at(InputAction::Escape, 80, start),
            AppCommand::None
        );
        assert_eq!(app.editor.text(), "unfinished draft");
        assert_eq!(
            app.handle_input_at(
                InputAction::Escape,
                80,
                start + DOUBLE_ESCAPE_WINDOW + Duration::from_millis(1),
            ),
            AppCommand::None
        );
        assert_eq!(app.editor.text(), "unfinished draft");
        assert_eq!(
            app.handle_input_at(
                InputAction::Escape,
                80,
                start + DOUBLE_ESCAPE_WINDOW + Duration::from_millis(100),
            ),
            AppCommand::None
        );
        assert!(app.editor.text().is_empty());
        assert!(app.escape_armed_at.is_none());
    }

    #[test]
    fn non_escape_input_disarms_draft_clear_while_busy_escape_still_interrupts() {
        let start = Instant::now();
        let mut app = model();
        app.editor.insert("keep me");
        let _ = app.handle_input_at(InputAction::Escape, 80, start);
        let _ = app.handle_input_at(InputAction::MoveLeft, 80, start + Duration::from_millis(10));
        let _ = app.handle_input_at(InputAction::Escape, 80, start + Duration::from_millis(20));
        assert_eq!(app.editor.text(), "keep me");

        app.begin_work();
        let _ = app.handle_input_at(
            InputAction::StashOrPop,
            80,
            start + Duration::from_millis(25),
        );
        assert!(app.editor.text().is_empty());
        assert!(app.has_stashed_draft());
        let _ = app.handle_input_at(
            InputAction::StashOrPop,
            80,
            start + Duration::from_millis(26),
        );
        assert_eq!(app.editor.text(), "keep me");
        assert_eq!(
            app.handle_input_at(InputAction::Escape, 80, start + Duration::from_millis(30)),
            AppCommand::CancelWork
        );
        assert_eq!(app.editor.text(), "keep me");
        assert!(app.escape_armed_at.is_none());
    }

    #[test]
    fn ctrl_s_stash_pop_is_last_wins_atomic_and_debug_redacted() {
        let first = (1..=12)
            .map(|line| format!("SECRET_PASTE_{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut app = model();
        let _ = app.handle_input(InputAction::Paste(first.clone()), 80);
        let _ = app
            .attach_image("image/png", "SECRET_STASH_IMAGE", 2048)
            .expect("attach image");

        let _ = app.handle_input(InputAction::StashOrPop, 80);
        assert!(app.editor.text().is_empty());
        assert!(app.has_stashed_draft());
        let debug = format!("{app:?}");
        assert!(!debug.contains("SECRET_PASTE"));
        assert!(!debug.contains("SECRET_STASH_IMAGE"));
        assert!(debug.contains("payloads redacted"));

        let _ = app.handle_input(InputAction::StashOrPop, 80);
        let AppCommand::Submit(restored) = app.handle_input(InputAction::Submit, 80) else {
            panic!("restored atomic draft should submit");
        };
        assert_eq!(restored.prompt, first);
        assert_eq!(restored.pastes.len(), 1);
        assert_eq!(restored.images.len(), 1);

        app.editor.insert("older stash");
        let _ = app.handle_input(InputAction::StashOrPop, 80);
        app.editor.insert("replacement");
        let _ = app.handle_input(InputAction::StashOrPop, 80);
        assert!(app.editor.text().is_empty());
        let _ = app.handle_input(InputAction::StashOrPop, 80);
        assert_eq!(app.editor.text(), "replacement");
        assert!(!app.has_stashed_draft());
    }

    #[test]
    fn stash_survives_submit_and_double_escape_but_can_be_cleared_for_a_session_change() {
        let start = Instant::now();
        let mut app = model();
        app.editor.insert("saved");
        let _ = app.handle_input(InputAction::StashOrPop, 80);
        app.editor.insert("submitted");
        assert!(matches!(
            app.handle_input(InputAction::Submit, 80),
            AppCommand::Submit(_)
        ));
        assert!(app.has_stashed_draft());

        app.editor.insert("throw away");
        let _ = app.handle_input_at(InputAction::Escape, 80, start);
        let _ = app.handle_input_at(InputAction::Escape, 80, start + Duration::from_millis(1));
        assert!(app.editor.text().is_empty());
        assert!(app.has_stashed_draft());

        app.clear_stashed_draft();
        assert!(!app.has_stashed_draft());
    }

    #[test]
    fn reverse_search_filters_and_navigates_without_a_second_editable_buffer() {
        let mut app = model();
        app.seed_history(vec![
            "alpha old command".to_string(),
            "beta old command".to_string(),
            "newest prompt".to_string(),
        ]);
        app.editor.insert("draft");

        assert_eq!(
            app.handle_input(InputAction::OpenReverseHistory, 80),
            AppCommand::None
        );
        assert_eq!(app.editor.text(), "newest prompt");
        assert!(app.reverse_search().is_some());

        let _ = app.handle_input(InputAction::Insert("old".to_string()), 80);
        assert_eq!(app.editor.text(), "beta old command");
        let _ = app.handle_input(InputAction::OpenReverseHistory, 80);
        assert_eq!(app.editor.text(), "alpha old command");
        let _ = app.handle_input(InputAction::MoveDown, 80);
        assert_eq!(app.editor.text(), "beta old command");

        assert_eq!(app.handle_input(InputAction::Escape, 80), AppCommand::None);
        assert!(!app.has_input_overlay());
        assert_eq!(app.editor.text(), "beta old command");
        app.editor.insert("!");
        assert_eq!(app.editor.text(), "beta old command!");
    }

    #[test]
    fn reverse_search_no_match_restores_the_original_draft_and_caret() {
        let mut app = model();
        app.seed_history(vec!["remembered".to_string()]);
        app.editor.insert("draft");
        app.editor.move_left();
        let cursor = app.editor.cursor();
        let _ = app.handle_input(InputAction::OpenReverseHistory, 80);
        let _ = app.handle_input(InputAction::Insert("absent".to_string()), 80);
        assert_eq!(app.editor.text(), "draft");
        assert_eq!(app.editor.cursor(), cursor);
    }

    #[test]
    fn reverse_search_no_match_restores_a_compact_paste_unit() {
        let payload = (1..=12)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut app = model();
        app.seed_history(vec!["remembered".to_string()]);
        let _ = app.handle_input(InputAction::Paste(payload.clone()), 80);
        let _ = app.handle_input(InputAction::OpenReverseHistory, 80);
        let _ = app.handle_input(InputAction::Insert("absent".to_string()), 80);
        assert_eq!(app.editor.text(), "[Paste #1 - 12 lines]");
        let _ = app.handle_input(InputAction::Escape, 80);

        let AppCommand::Submit(submitted) = app.handle_input(InputAction::Submit, 80) else {
            panic!("restored paste should submit");
        };
        assert_eq!(submitted.prompt, payload);
        assert_eq!(submitted.pastes.len(), 1);
    }

    #[test]
    fn overlay_escape_precedes_work_interrupt_and_dialog_precedes_the_overlay() {
        let mut app = model();
        app.seed_history(vec!["remembered".to_string()]);
        app.begin_work();
        let _ = app.handle_input(InputAction::OpenReverseHistory, 80);
        assert!(app.has_input_overlay());

        assert_eq!(app.handle_input(InputAction::Escape, 80), AppCommand::None);
        assert_eq!(
            app.active_work(),
            WorkState::Busy {
                cancellation_requested: false
            }
        );
        assert_eq!(
            app.handle_input(InputAction::Escape, 80),
            AppCommand::CancelWork
        );

        let mut dialog = model();
        dialog.seed_history(vec!["remembered".to_string()]);
        dialog.require_workspace_trust("fixture");
        let _ = dialog.handle_input(InputAction::OpenReverseHistory, 80);
        assert!(!dialog.has_input_overlay());
    }

    #[test]
    fn stash_shortcut_is_contained_by_overlays_takeovers_themes_and_dialogs() {
        let mut overlay = model();
        overlay.seed_history(vec!["remembered".to_string()]);
        overlay.editor.insert("draft");
        let _ = overlay.handle_input(InputAction::OpenReverseHistory, 80);
        let _ = overlay.handle_input(InputAction::StashOrPop, 80);
        assert!(overlay.has_input_overlay());
        assert!(!overlay.has_stashed_draft());

        let mut takeover = model();
        takeover.editor.insert("draft");
        takeover.open_help();
        let _ = takeover.handle_input(InputAction::StashOrPop, 80);
        assert!(takeover.has_takeover());
        assert!(!takeover.has_stashed_draft());

        let mut theme = model();
        theme.editor.insert("draft");
        theme.open_theme_picker();
        let _ = theme.handle_input(InputAction::StashOrPop, 80);
        assert!(theme.theme_picker().is_some());
        assert!(!theme.has_stashed_draft());

        let mut dialog = model();
        dialog.editor.insert("draft");
        dialog.require_workspace_trust("workspace");
        let _ = dialog.handle_input(InputAction::StashOrPop, 80);
        assert!(dialog.dialog.is_some());
        assert!(!dialog.has_stashed_draft());
    }

    #[test]
    fn opening_an_overlay_disarms_contextual_exit() {
        let mut app = model();
        app.seed_history(vec!["remembered".to_string()]);
        app.exit_armed = true;
        let _ = app.handle_input(InputAction::OpenReverseHistory, 80);
        assert!(!app.exit_armed);
        assert!(app.has_input_overlay());
    }

    #[test]
    fn reverse_search_enter_submits_the_match_and_ctrl_c_only_dismisses() {
        let mut enter = model();
        enter.seed_history(vec!["matched prompt".to_string()]);
        let _ = enter.handle_input(InputAction::OpenReverseHistory, 80);
        let AppCommand::Submit(submitted) = enter.handle_input(InputAction::Submit, 80) else {
            panic!("reverse search Enter should submit its match");
        };
        assert_eq!(submitted.prompt, "matched prompt");
        assert!(!enter.has_input_overlay());

        let mut cancel = model();
        cancel.seed_history(vec!["matched prompt".to_string()]);
        let _ = cancel.handle_input(InputAction::OpenReverseHistory, 80);
        assert_eq!(
            cancel.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::None
        );
        assert!(!cancel.has_input_overlay());
        assert!(!cancel.exit_armed);
        assert!(!cancel.exit_requested);
    }

    #[test]
    fn slash_completion_fuzzy_filters_and_accepts_without_submitting() {
        let mut app = model();
        app.set_command_catalog([
            command("model", "Switch model"),
            command("knowledge", "Query knowledge"),
            command("compact", "Compact context"),
        ]);
        assert_eq!(
            app.handle_input(InputAction::Insert("/knw".to_string()), 80),
            AppCommand::None
        );
        let completion = app.completion().expect("completion");
        assert_eq!(completion.items.len(), 1);
        assert_eq!(completion.items[0].name, "knowledge");

        assert_eq!(app.handle_input(InputAction::Submit, 80), AppCommand::None);
        assert_eq!(app.editor.text(), "/knowledge");
        assert!(!app.has_input_overlay());
        let AppCommand::RunSlash(submitted) = app.handle_input(InputAction::Submit, 80) else {
            panic!("closed-picker Enter should execute the slash command");
        };
        assert_eq!(submitted.prompt, "/knowledge");
    }

    #[test]
    fn slash_completion_navigation_accept_keys_and_escape_are_contained() {
        let mut app = model();
        app.set_command_catalog([
            command("model", "Switch model"),
            command("memory", "Inspect memory"),
            command("remote", "Remote command fixture"),
        ]);
        let _ = app.handle_input(InputAction::Insert("/mo".to_string()), 80);
        assert_eq!(app.completion().expect("completion").items[0].name, "model");
        let _ = app.handle_input(InputAction::MoveDown, 80);
        assert_eq!(app.completion().expect("completion").selected, 1);
        let _ = app.handle_input(InputAction::AcceptCompletion, 80);
        assert_eq!(app.editor.text(), "/memory");

        app.editor.replace_draft("");
        let _ = app.handle_input(InputAction::Insert("/mo".to_string()), 80);
        let _ = app.handle_input(InputAction::Escape, 80);
        assert_eq!(app.editor.text(), "/mo");
        assert!(!app.has_input_overlay());
    }

    #[test]
    fn model_command_acceptance_opens_one_contained_value_picker() {
        let mut app = model();
        app.set_command_catalog([command("model", "Switch model")]);
        app.set_command_values(
            "model",
            [
                command("local", "Local provider"),
                command("remote", "Remote provider"),
            ],
        );

        let _ = app.handle_input(InputAction::Insert("/mo".to_string()), 80);
        assert_eq!(
            app.handle_input(InputAction::AcceptCompletion, 80),
            AppCommand::None
        );
        let picker = app.completion().expect("model value picker");
        assert_eq!(picker.kind, CompletionKind::CommandValue);
        assert_eq!(picker.items.len(), 2);
        assert_eq!(app.editor.text(), "/model ");

        let _ = app.handle_input(InputAction::MoveDown, 80);
        let AppCommand::RunSlash(submitted) = app.handle_input(InputAction::Submit, 80) else {
            panic!("a selected model value should execute as a slash command");
        };
        assert_eq!(submitted.prompt, "/model remote");
        assert!(app.completion().is_none());

        app.editor.replace_draft("/model");
        assert_eq!(app.handle_input(InputAction::Submit, 80), AppCommand::None);
        assert!(app.completion().is_some());
        let _ = app.handle_input(InputAction::Escape, 80);
        assert_eq!(app.editor.text(), "/model ");
        assert!(app.completion().is_none());

        app.editor.replace_draft("/model rem");
        assert_eq!(app.handle_input(InputAction::Submit, 80), AppCommand::None);
        let picker = app.completion().expect("filtered value picker");
        assert_eq!(picker.items.len(), 1);
        assert_eq!(picker.items[0].name, "remote");

        let _ = app.handle_input(InputAction::Escape, 80);
        app.editor.replace_draft("/model local");
        let AppCommand::RunSlash(submitted) = app.handle_input(InputAction::Submit, 80) else {
            panic!("an exact provider should execute directly");
        };
        assert_eq!(submitted.prompt, "/model local");

        app.editor.replace_draft("");
        let _ = app.handle_input(InputAction::Insert("/model".to_string()), 80);
        assert_eq!(
            app.completion().expect("command picker").kind,
            CompletionKind::Command
        );
        let _ = app.handle_input(InputAction::Insert(" ".to_string()), 80);
        assert_eq!(
            app.completion().expect("space boundary value picker").kind,
            CompletionKind::CommandValue
        );
    }

    #[test]
    fn slash_submission_is_distinct_from_a_model_prompt() {
        let mut app = model();
        app.seed_history(vec!["ordinary prompt".to_string()]);
        let _ = app.handle_input(InputAction::Insert("/clear".to_string()), 80);
        let AppCommand::RunSlash(submitted) = app.handle_input(InputAction::Submit, 80) else {
            panic!("slash input must not become a provider prompt");
        };
        assert_eq!(submitted.prompt, "/clear");
        assert!(!format!("{submitted:?}").contains("/clear"));
        let _ = app.handle_input(InputAction::MoveUp, 80);
        assert_eq!(app.editor.text(), "ordinary prompt");

        let mut image = model();
        let _ = image.handle_input(InputAction::Insert("/clear".to_string()), 80);
        assert!(image
            .attach_image("image/png", "IMAGE_SECRET", 128)
            .is_some());
        assert_eq!(
            image.handle_input(InputAction::Submit, 80),
            AppCommand::None
        );
        assert_eq!(image.editor.text(), "/clear[image #1 · PNG 128 B]");
        assert!(image.active_timeline().items().iter().any(|item| {
            item.kind == ItemKind::Notice && item.text.contains("remove image attachments")
        }));
    }

    #[test]
    fn research_mode_rejects_a_prompt_with_images_and_preserves_the_draft() {
        let mut app = model();
        app.seed_history(vec!["an earlier prompt".to_string()]);
        app.set_shared_mode(Mode::Research);
        // A COMPLETE draft: text + a pasted unit + an image + a non-terminal cursor.
        let _ = app.handle_input(InputAction::Insert("a research topic".to_string()), 80);
        app.editor.insert_paste("PASTED-BLOCK");
        assert!(app.attach_image("image/png", "IMAGE_SECRET", 128).is_some());
        app.editor.set_cursor_from_visual(0, 4, 80); // mid-line, non-terminal cursor
        let before = app.editor.snapshot();
        let timeline_before = app.active_timeline().items().len();
        // Research is text-only: the prompt is rejected BEFORE `submit()` consumes the
        // draft — no submission, one notice, and no durable side effect.
        assert_eq!(app.handle_input(InputAction::Submit, 80), AppCommand::None);
        assert!(
            app.editor.snapshot() == before,
            "the EXACT before-consume editor state (text + paste unit + image + cursor + \
             shell_mode) is preserved on rejection"
        );
        // A consumed prompt would clear the editor and append to prompt history; the
        // intact snapshot proves neither happened — nothing was recalled or persisted.
        // (`text()` also carries the paste placeholder, so match a substring.)
        assert!(app.editor.has_images() && app.editor.text().contains("a research topic"));
        assert!(app.active_timeline().items().iter().any(|item| {
            item.kind == ItemKind::Notice && item.text.contains("research is text-only")
        }));
        assert_eq!(
            app.active_timeline().items().len(),
            timeline_before + 1,
            "only the rejection notice was added — no user prompt row"
        );
        assert_eq!(app.active_work(), WorkState::Idle, "no worker started");

        // History: the rejected Research prompt never entered prompt recall. Clear the
        // draft through the editor seam and recall once — the only entry is the seeded
        // prior prompt (a submitted prompt would have appended "a research topic…").
        app.editor.clear_all();
        let _ = app.handle_input(InputAction::MoveUp, 80);
        assert_eq!(
            app.editor.text(),
            "an earlier prompt",
            "recall surfaces only the seeded prior prompt — the rejected prompt never entered history"
        );
    }

    #[test]
    fn research_slash_forms_with_images_hit_the_generic_slash_rejection() {
        // `/research <topic>` and bare `/research` carrying images are declined by the
        // generic images-before-slash guard (not the research-plain guard) BEFORE submit,
        // preserving the full editor state — no user row, `AppCommand::None`, Idle.
        for line in ["/research a topic", "/research"] {
            let mut app = model();
            app.set_shared_mode(Mode::Research);
            let _ = app.handle_input(InputAction::Insert(line.to_string()), 80);
            assert!(app.attach_image("image/png", "IMG", 64).is_some());
            let before = app.editor.snapshot();
            let timeline_before = app.active_timeline().items().len();
            assert_eq!(
                app.handle_input(InputAction::Submit, 80),
                AppCommand::None,
                "{line} with images is declined"
            );
            assert!(
                app.editor.snapshot() == before,
                "{line}: the full draft + image are preserved"
            );
            assert!(
                app.active_timeline().items().iter().any(|item| {
                    item.kind == ItemKind::Notice
                        && item
                            .text
                            .contains("remove image attachments before running a slash command")
                }),
                "{line}: the generic slash-image rejection notice"
            );
            assert_eq!(
                app.active_timeline().items().len(),
                timeline_before + 1,
                "{line}: only the notice — no user row"
            );
            assert_eq!(app.active_work(), WorkState::Idle);
        }
    }

    #[test]
    fn set_shared_mode_updates_the_typed_authority_and_composer_hint() {
        let mut app = model();
        assert_eq!(app.mode(), Mode::Agent);
        assert_eq!(app.shared_mode(), "agent");
        assert_eq!(app.composer_hint(), None);
        app.set_shared_mode(Mode::Research);
        assert_eq!(app.mode(), Mode::Research);
        assert_eq!(
            app.shared_mode(),
            "research",
            "footer/settings project the label"
        );
        assert_eq!(
            app.composer_hint(),
            Some("Research a topic — local + web per config")
        );
        app.set_shared_mode(Mode::Agent);
        assert_eq!(app.mode(), Mode::Agent);
        assert_eq!(app.composer_hint(), None);
    }

    #[test]
    fn help_is_a_contained_history_free_takeover_during_idle_or_work() {
        let mut app = model();
        app.set_command_catalog([
            command("help", "Open help"),
            command("clear", "Clear conversation"),
        ]);
        app.seed_history(vec!["ordinary prompt".to_string()]);
        let existing = app
            .active_timeline_mut()
            .push(ItemKind::Assistant, "conversation remains underneath")
            .expect("timeline item");
        app.begin_work();
        app.editor.replace_draft("/help");

        // The widget now emits `/help` as an ordinary slash command; the host
        // opens the contained, history-free help takeover (including mid-work).
        let command = app.handle_input(InputAction::Submit, 80);
        let AppCommand::RunSlash(submitted) = command else {
            panic!("expected RunSlash, got {command:?}");
        };
        assert_eq!(submitted.prompt, "/help");
        assert!(submitted.images.is_empty());
        assert!(app.editor.text().is_empty());
        app.open_help();
        assert!(app.has_takeover());
        assert_eq!(
            app.active_work(),
            WorkState::Busy {
                cancellation_requested: false
            }
        );
        assert!(app.active_timeline().item(existing).is_some());
        assert!(app.attach_image("image/png", "IMAGE_SECRET", 128).is_none());
        assert_eq!(
            app.handle_input(InputAction::MoveDown, 80),
            AppCommand::NavigateTakeover(TakeoverNavigation::LineDown)
        );

        app.apply_runtime(RuntimeUpdate::Text("streamed behind help".to_string()));
        assert_eq!(app.handle_input(InputAction::Escape, 80), AppCommand::None);
        assert!(!app.has_takeover());
        assert_eq!(
            app.active_work(),
            WorkState::Busy {
                cancellation_requested: false
            }
        );
        assert!(app
            .active_timeline()
            .items()
            .iter()
            .any(|item| item.text.contains("streamed behind help")));

        app.apply_runtime(RuntimeUpdate::Stopped(StopState::Done));
        let _ = app.handle_input(InputAction::MoveUp, 80);
        assert_eq!(app.editor.text(), "ordinary prompt");
    }

    #[test]
    fn ctrl_c_closes_help_without_arming_or_exiting() {
        let mut app = model();
        app.open_help();
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::None
        );
        assert!(!app.has_takeover());
        assert!(!app.exit_armed);
        assert!(!app.exit_requested);
    }

    #[test]
    fn settings_takeover_owns_navigation_and_sanitizes_host_values() {
        let mut app = model();
        app.open_settings([
            SettingEntry {
                section: "Input".to_string(),
                name: "Mouse".to_string(),
                value: "On".to_string(),
                description: "Captured input".to_string(),
                edit: None,
                is_default: true,
            },
            SettingEntry {
                section: "Session".to_string(),
                name: "Model".to_string(),
                value: "unsafe\u{1b}[2Jmodel\nnext\tvalue".to_string(),
                description: "Current model".to_string(),
                edit: None,
                is_default: true,
            },
        ]);

        assert_eq!(app.takeover().expect("settings").selected, 0);
        assert_eq!(
            app.handle_input(InputAction::MoveDown, 80),
            AppCommand::NavigateTakeover(TakeoverNavigation::LineDown)
        );
        app.scroll_takeover_by(1, 2, 1);
        let view = app.takeover().expect("settings");
        assert_eq!(view.selected, 1);
        assert_eq!(view.scroll, 1);
        assert_eq!(view.settings[1].value, "unsafemodel next value");
        assert_eq!(app.handle_input(InputAction::Escape, 80), AppCommand::None);
        assert!(!app.has_takeover());
    }

    #[test]
    fn settings_search_filters_clears_before_close_and_redacts_debug_text() {
        let mut app = model();
        app.open_settings([
            SettingEntry {
                section: "Input".into(),
                name: "Copy on selection".into(),
                value: "Off".into(),
                description: "Copy selected text".into(),
                edit: Some(SettingEdit::CopyOnSelect),
                is_default: true,
            },
            SettingEntry {
                section: "Appearance".into(),
                name: "Color mode".into(),
                value: "Default".into(),
                description: "Choose colors".into(),
                edit: Some(SettingEdit::Theme),
                is_default: true,
            },
        ]);

        for hotkey in ["a", "i", "r", "p"] {
            assert_eq!(
                app.handle_input(InputAction::Insert(hotkey.to_string()), 80),
                AppCommand::None
            );
        }
        assert_eq!(
            app.takeover().expect("settings").settings_query,
            "airp",
            "LocalMind review hotkeys must remain ordinary Settings filter text"
        );
        assert_eq!(app.handle_input(InputAction::Escape, 80), AppCommand::None);
        assert!(app.has_takeover());

        assert_eq!(
            app.handle_input(InputAction::Insert("PLANTED_COLOR_QUERY".into()), 80),
            AppCommand::None
        );
        let state = app.takeover.as_ref().expect("settings state");
        assert_eq!(filtered_setting_indices(state), Vec::<usize>::new());
        let debug = format!("{state:?}");
        assert!(!debug.contains("PLANTED_COLOR_QUERY"));
        assert!(debug.contains("<19 bytes redacted>"));

        assert_eq!(app.handle_input(InputAction::Escape, 80), AppCommand::None);
        assert!(app.has_takeover());
        assert!(app.takeover().expect("settings").settings_query.is_empty());
        assert_eq!(app.handle_input(InputAction::Escape, 80), AppCommand::None);
        assert!(!app.has_takeover());
    }

    #[test]
    fn editable_settings_toggle_reset_and_theme_picker_returns_to_settings() {
        let mut app = model();
        app.capture_setting_defaults();
        app.open_settings([
            SettingEntry {
                section: "Input".into(),
                name: "Copy on selection".into(),
                value: "Off".into(),
                description: "Copy selected text".into(),
                edit: Some(SettingEdit::CopyOnSelect),
                is_default: true,
            },
            SettingEntry {
                section: "Appearance".into(),
                name: "Color mode".into(),
                value: "Default".into(),
                description: "Choose colors".into(),
                edit: Some(SettingEdit::Theme),
                is_default: true,
            },
        ]);

        assert_eq!(app.handle_input(InputAction::Submit, 80), AppCommand::None);
        assert!(app.copy_on_select());
        assert_eq!(app.takeover().expect("settings").settings[0].value, "On");
        assert!(!app.takeover().expect("settings").settings[0].is_default);
        assert_eq!(
            app.handle_input(InputAction::OpenReverseHistory, 80),
            AppCommand::None
        );
        assert!(!app.copy_on_select());
        assert!(app.takeover().expect("settings").settings[0].is_default);

        app.scroll_takeover_by(1, 2, 2);
        assert_eq!(app.handle_input(InputAction::Submit, 80), AppCommand::None);
        assert!(app.has_theme_picker());
        assert!(!app.has_takeover());
        let _ = app.handle_input(InputAction::MoveDown, 80);
        assert_eq!(app.theme, Theme::Dim);
        let _ = app.handle_input(InputAction::Submit, 80);
        assert!(!app.has_theme_picker());
        assert!(app.has_takeover());
        let view = app.takeover().expect("returned settings");
        assert_eq!(view.selected, 1);
        assert_eq!(view.settings[1].value, "Dim");
        assert!(!view.settings[1].is_default);

        let _ = app.handle_input(InputAction::OpenReverseHistory, 80);
        assert_eq!(app.theme, Theme::Default);
        assert_eq!(
            app.takeover().expect("reset settings").settings[1].value,
            "Default"
        );
    }

    #[test]
    fn sessions_takeover_selects_current_sanitizes_values_and_returns_a_typed_action() {
        let mut app = model();
        app.open_sessions([
            SessionEntry {
                selector: "first-selector".to_string(),
                name: Some("PLANTED_FIRST_NAME".to_string()),
                message_count: 3,
                updated: Some("2026-08-01 10:00".to_string()),
                current: false,
            },
            SessionEntry {
                selector: "current\u{1b}[2J-selector".to_string(),
                name: Some("PLANTED_CURRENT_NAME\nnext".to_string()),
                message_count: 7,
                updated: Some("2026-08-01 09:00".to_string()),
                current: true,
            },
        ]);

        let view = app.takeover().expect("sessions");
        assert_eq!(view.kind, TakeoverKind::Sessions);
        assert_eq!(view.selected, 1);
        assert_eq!(view.scroll, 1);
        assert_eq!(view.sessions[1].selector, "current-selector");
        assert_eq!(
            view.sessions[1].name.as_deref(),
            Some("PLANTED_CURRENT_NAME next")
        );
        let debug = format!("{:?}", view.sessions);
        assert!(!debug.contains("PLANTED_FIRST_NAME"));
        assert!(!debug.contains("current-selector"));

        app.select_takeover_row(0);
        let AppCommand::ActivateSession(selection) = app.handle_input(InputAction::Submit, 80)
        else {
            panic!("session Enter should return a typed activation");
        };
        assert_eq!(selection.as_str(), "first-selector");
        assert!(!format!("{selection:?}").contains("first-selector"));
        assert!(!app.has_takeover());

        app.open_sessions(std::iter::empty());
        assert_eq!(app.handle_input(InputAction::Submit, 80), AppCommand::None);
        assert!(app.has_takeover());
        assert_eq!(app.handle_input(InputAction::Escape, 80), AppCommand::None);
        assert!(!app.has_takeover());
    }

    #[test]
    fn diff_takeover_routes_content_file_and_tree_focus_without_touching_chat() {
        let mut app = model();
        let existing = app
            .active_timeline_mut()
            .push(ItemKind::Assistant, "conversation")
            .expect("timeline item");
        let file = |path: &str| DiffFile {
            status: "M".to_string(),
            path: path.to_string(),
            additions: 1,
            deletions: 0,
            lines: vec![
                DiffLine {
                    old_line: None,
                    new_line: None,
                    kind: DiffLineKind::Hunk,
                    text: "@@ -1 +1 @@".to_string(),
                },
                DiffLine {
                    old_line: None,
                    new_line: Some(1),
                    kind: DiffLineKind::Addition,
                    text: "DIFF_SECRET_CONTENT".to_string(),
                },
            ],
        };
        app.open_diff([file("one.rs"), file("two.rs")]);
        let debug = format!("{app:?}");
        assert!(!debug.contains("one.rs"));
        assert!(!debug.contains("DIFF_SECRET_CONTENT"));

        assert_eq!(
            app.handle_input(InputAction::MoveDown, 80),
            AppCommand::NavigateTakeover(TakeoverNavigation::LineDown)
        );
        app.scroll_takeover_by(1, 2, 1);
        assert_eq!(app.takeover().expect("diff").selected, 1);
        let _ = app.handle_input(InputAction::MoveLeft, 80);
        assert_eq!(
            app.handle_input(InputAction::MoveDown, 80),
            AppCommand::NavigateTakeover(TakeoverNavigation::LineDown)
        );
        app.scroll_takeover_by(1, 2, 1);
        let view = app.takeover().expect("diff");
        assert_eq!(view.selected_file, 1);
        assert_eq!(view.selected, 1);
        assert_eq!(view.file_scroll, 1);
        let _ = app.handle_input(InputAction::Insert("t".to_string()), 80);
        let view = app.takeover().expect("diff");
        assert!(!view.tree_visible);
        assert_eq!(view.diff_pane, DiffPane::Content);
        let _ = app.handle_input(InputAction::Escape, 80);
        assert!(app.active_timeline().item(existing).is_some());
        assert!(!app.has_takeover());
    }

    #[test]
    fn empty_question_mark_toggles_transient_quick_help_without_editing() {
        let mut app = model();
        assert_eq!(
            app.handle_input(InputAction::Insert("?".to_string()), 80),
            AppCommand::None
        );
        assert!(app.quick_help());
        assert!(app.editor.text().is_empty());

        let _ = app.handle_input(InputAction::Insert("?".to_string()), 80);
        assert!(!app.quick_help());
        assert!(app.editor.text().is_empty());

        let _ = app.handle_input(InputAction::Insert("?".to_string()), 80);
        let _ = app.handle_input(InputAction::Insert("x".to_string()), 80);
        assert!(!app.quick_help());
        assert_eq!(app.editor.text(), "x");

        app.editor.replace_draft("");
        app.begin_work();
        let _ = app.handle_input(InputAction::Insert("?".to_string()), 80);
        assert!(!app.quick_help());
        assert_eq!(app.editor.text(), "?");
    }

    #[test]
    fn quick_help_does_not_preempt_selection_copy() {
        let mut app = model();
        let item = app
            .active_timeline_mut()
            .push(ItemKind::Assistant, "copy me")
            .expect("timeline item");
        app.active_timeline_mut().start_selection(ContentPoint {
            item_id: item,
            byte: 0,
        });
        app.active_timeline_mut().extend_selection(ContentPoint {
            item_id: item,
            byte: "copy".len(),
        });
        let _ = app.handle_input(InputAction::Insert("?".to_string()), 80);

        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::Copy("copy".to_string())
        );
        assert!(app.quick_help());
        assert!(app.exit_armed);
    }

    #[test]
    fn theme_picker_previews_cancels_or_accepts_without_history_or_work_leakage() {
        let mut app = model();
        app.seed_history(vec!["ordinary prompt".to_string()]);
        app.begin_work();
        app.editor.replace_draft("/theme");
        // `/theme` is emitted as an ordinary slash command; the host opens the
        // picker (drive it directly here to test the picker's containment).
        let command = app.handle_input(InputAction::Submit, 80);
        let AppCommand::RunSlash(submitted) = command else {
            panic!("expected RunSlash, got {command:?}");
        };
        assert_eq!(submitted.prompt, "/theme");
        app.open_theme_picker();
        assert!(app.has_theme_picker());
        assert_eq!(app.theme, Theme::Default);
        assert!(app.attach_image("image/png", "IMAGE_SECRET", 128).is_none());

        let _ = app.handle_input(InputAction::MoveDown, 80);
        assert_eq!(app.theme, Theme::Dim);
        assert_eq!(app.handle_input(InputAction::Escape, 80), AppCommand::None);
        assert!(!app.has_theme_picker());
        assert_eq!(app.theme, Theme::Default);
        assert_eq!(
            app.active_work(),
            WorkState::Busy {
                cancellation_requested: false
            }
        );

        app.apply_runtime(RuntimeUpdate::Stopped(StopState::Done));
        app.open_theme_picker();
        let _ = app.handle_input(InputAction::MoveDown, 80);
        let _ = app.handle_input(InputAction::Submit, 80);
        assert!(!app.has_theme_picker());
        assert_eq!(app.theme, Theme::Dim);
        let _ = app.handle_input(InputAction::MoveUp, 80);
        assert_eq!(app.editor.text(), "ordinary prompt");
    }

    #[test]
    fn apply_theme_by_value_supersedes_an_open_picker() {
        let mut app = model();
        // No picker open: the theme is applied directly.
        app.apply_theme(Theme::Dim);
        assert_eq!(app.theme, Theme::Dim);
        assert!(!app.has_theme_picker());

        // A picker previewing another theme must not clobber the requested one:
        // the picker is closed/restored first, then the requested theme applies.
        app.open_theme_picker();
        let _ = app.handle_input(InputAction::MoveDown, 80);
        app.apply_theme(Theme::HighContrast);
        assert!(!app.has_theme_picker());
        assert_eq!(app.theme, Theme::HighContrast);
    }

    #[test]
    fn open_settings_with_query_prefills_and_caps_the_filter() {
        let entry = |name: &str| SettingEntry {
            section: "Input".into(),
            name: name.into(),
            value: "On".into(),
            description: "Setting".into(),
            edit: None,
            is_default: true,
        };
        let mut app = model();
        app.open_settings_with_query([entry("Mouse")], "mouse");
        assert!(app.has_takeover());
        assert_eq!(app.takeover().expect("settings").settings_query, "mouse");

        // An over-long query is capped at the byte budget, not stored whole.
        let long = "x".repeat(MAX_SETTINGS_QUERY_BYTES * 2);
        app.open_settings_with_query([entry("Mouse")], &long);
        assert!(app.takeover().expect("settings").settings_query.len() <= MAX_SETTINGS_QUERY_BYTES);
    }

    #[test]
    fn takeover_slash_with_images_is_non_silent_and_retained() {
        // Deleting the widget interceptions must not silently drop pasted images:
        // `/help`, `/theme`, and `/search` fall to the generic slash guard, which
        // warns, retains the attachment, and does not run the command.
        for command in ["/help", "/theme", "/search foo"] {
            let mut app = model();
            let _ = app.handle_input(InputAction::Insert(command.to_string()), 80);
            assert!(app.attach_image("image/png", "IMAGE_SECRET", 128).is_some());
            assert_eq!(app.handle_input(InputAction::Submit, 80), AppCommand::None);
            assert!(!app.has_takeover(), "{command} must not open a takeover");
            assert!(!app.has_theme_picker());
            assert!(app.timeline_search().is_none());
            assert!(
                app.active_timeline().items().iter().any(|item| {
                    item.kind == ItemKind::Notice && item.text.contains("remove image attachments")
                }),
                "{command} must warn about attachments",
            );
            assert!(
                app.editor.text().contains(command),
                "{command} draft retained"
            );
            assert!(
                app.editor.has_images(),
                "{command} must retain the attachment, not just the text"
            );
        }
    }

    #[test]
    fn ctrl_c_cancels_theme_preview_without_arming_exit() {
        let mut app = model();
        app.open_theme_picker();
        let _ = app.handle_input(InputAction::MoveDown, 80);
        assert_eq!(app.theme, Theme::Dim);
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::None
        );
        assert_eq!(app.theme, Theme::Default);
        assert!(!app.has_theme_picker());
        assert!(!app.exit_armed);
        assert!(!app.exit_requested);
    }

    #[test]
    fn completion_escape_precedes_busy_work_interrupt() {
        let mut app = model();
        app.set_command_catalog([command("model", "Switch model")]);
        app.begin_work();
        let _ = app.handle_input(InputAction::Insert("/mo".to_string()), 80);
        assert!(app.has_input_overlay());
        assert_eq!(app.handle_input(InputAction::Escape, 80), AppCommand::None);
        assert_eq!(
            app.active_work(),
            WorkState::Busy {
                cancellation_requested: false
            }
        );
        assert_eq!(
            app.handle_input(InputAction::Escape, 80),
            AppCommand::CancelWork
        );
    }

    #[test]
    fn mention_completion_stays_live_while_indexing_then_accepts_a_file() {
        let mut app = model();
        let _ = app.handle_input(InputAction::Insert("open @sam".to_string()), 80);
        let completion = app.completion().expect("loading mention");
        assert_eq!(completion.kind, CompletionKind::File);
        assert!(completion.loading);
        assert!(completion.items.is_empty());
        assert_eq!(app.handle_input(InputAction::Submit, 80), AppCommand::None);
        assert!(app.has_input_overlay());

        app.set_workspace_files([
            "README.md".to_string(),
            "src/sample.rs".to_string(),
            "src/state.rs".to_string(),
        ]);
        let completion = app.completion().expect("ready mention");
        assert!(!completion.loading);
        assert_eq!(completion.items.len(), 1);
        assert_eq!(completion.items[0].name, "src/sample.rs");

        assert_eq!(app.handle_input(InputAction::Submit, 80), AppCommand::None);
        assert_eq!(app.editor.text(), "open @src/sample.rs ");
        assert!(!app.has_input_overlay());
    }

    #[test]
    fn mention_escape_keeps_the_original_token_editable() {
        let mut app = model();
        app.set_workspace_files(["sample.rs".to_string()]);
        let _ = app.handle_input(InputAction::Insert("@sam".to_string()), 80);
        assert!(app.has_input_overlay());
        let _ = app.handle_input(InputAction::Escape, 80);
        assert_eq!(app.editor.text(), "@sam");
        assert!(!app.has_input_overlay());
    }

    #[test]
    fn image_attach_respects_overlay_ownership_and_selection_copies_only_placeholder_text() {
        let mut app = model();
        app.set_command_catalog([command("model", "Switch model")]);
        let _ = app.handle_input(InputAction::Insert("/mo".to_string()), 80);
        assert!(app.has_input_overlay());
        assert_eq!(
            app.attach_image("image/png", "BLOCKED_IMAGE_DATA", 32),
            None
        );
        let _ = app.handle_input(InputAction::Escape, 80);
        app.editor.replace_draft("");

        let placeholder = app
            .attach_image("image/png", "SECRET_IMAGE_DATA", 2048)
            .expect("attached image");
        assert_eq!(app.editor.text(), placeholder);
        assert!(!format!("{app:?}").contains("SECRET_IMAGE_DATA"));
        let AppCommand::Submit(submitted) = app.handle_input(InputAction::Submit, 80) else {
            panic!("image placeholder should submit");
        };
        assert_eq!(submitted.prompt, "");
        assert_eq!(submitted.images.len(), 1);
        let id = app
            .append_prompt(submitted.display.clone(), None, false)
            .expect("prompt");
        app.active_timeline_mut().start_selection(ContentPoint {
            item_id: id,
            byte: 0,
        });
        app.active_timeline_mut().extend_selection(ContentPoint {
            item_id: id,
            byte: submitted.display.len(),
        });
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::Copy(placeholder)
        );
    }

    #[test]
    fn image_attach_block_names_the_owning_surface() {
        // A composer with an empty draft accepts an image.
        let idle = model();
        assert_eq!(idle.image_attach_block(), None);

        // Every reason has a non-empty notice.
        for block in [
            ImageAttachBlock::NotComposer,
            ImageAttachBlock::Dialog,
            ImageAttachBlock::Takeover,
            ImageAttachBlock::ThemePicker,
            ImageAttachBlock::InputOverlay,
            ImageAttachBlock::ShellMode,
        ] {
            assert!(!block.message().is_empty());
        }

        // Each of the six concrete owning surfaces reports its reason and refuses
        // the attach. NotComposer is the real Focus::Timeline state.
        let check = |setup: &dyn Fn(&mut AppModel), expected: ImageAttachBlock| {
            let mut app = model();
            setup(&mut app);
            assert_eq!(app.image_attach_block(), Some(expected));
            assert!(
                app.attach_image("image/png", "X", 4).is_none(),
                "attach must be refused for {expected:?}"
            );
        };
        check(
            &|app| app.focus = Focus::Timeline,
            ImageAttachBlock::NotComposer,
        );
        check(
            &|app| app.require_workspace_trust("fixture"),
            ImageAttachBlock::Dialog,
        );
        check(&|app| app.open_help(), ImageAttachBlock::Takeover);
        check(
            &|app| app.open_theme_picker(),
            ImageAttachBlock::ThemePicker,
        );
        check(
            &|app| {
                app.set_command_catalog([command("model", "Switch model")]);
                let _ = app.handle_input(InputAction::Insert("/mo".to_string()), 80);
            },
            ImageAttachBlock::InputOverlay,
        );
        check(
            &|app| {
                let _ = app.handle_input(InputAction::Insert("!echo".to_string()), 80);
            },
            ImageAttachBlock::ShellMode,
        );
    }

    #[test]
    fn external_edit_preserves_opaque_units_only_when_placeholder_text_is_unchanged() {
        let mut unchanged = model();
        let paste_secret = format!("{}\nline 2\nline 3\nline 4", "PASTE_SECRET");
        let _ = unchanged.handle_input(InputAction::Paste(paste_secret), 80);
        unchanged
            .attach_image("image/png", "IMAGE_SECRET", 2048)
            .expect("image attachment");
        assert_eq!(
            unchanged.handle_input(InputAction::OpenExternalEditor, 80),
            AppCommand::OpenExternalEditor
        );
        let external = unchanged
            .external_edit_text()
            .expect("external edit snapshot")
            .to_string();
        assert!(!external.contains("PASTE_SECRET"));
        assert!(!external.contains("IMAGE_SECRET"));
        unchanged.finish_external_edit(Some(format!("{external}\n")));
        let AppCommand::Submit(submitted) = unchanged.handle_input(InputAction::Submit, 80) else {
            panic!("restored atomic draft should submit");
        };
        assert_eq!(submitted.pastes.len(), 1);
        assert_eq!(submitted.images.len(), 1);

        let mut changed = model();
        let placeholder = changed
            .attach_image("image/png", "DROP_IMAGE_SECRET", 1024)
            .expect("image attachment");
        assert_eq!(
            changed.handle_input(InputAction::OpenExternalEditor, 80),
            AppCommand::OpenExternalEditor
        );
        changed.finish_external_edit(Some(placeholder.replace("image", "edited image")));
        let AppCommand::Submit(submitted) = changed.handle_input(InputAction::Submit, 80) else {
            panic!("edited literal draft should submit");
        };
        assert!(submitted.images.is_empty());
        assert!(submitted.pastes.is_empty());
        assert!(submitted.prompt.contains("edited image"));

        let mut failed = model();
        failed
            .attach_image("image/png", "RESTORED_IMAGE_SECRET", 512)
            .expect("image attachment");
        assert_eq!(
            failed.handle_input(InputAction::OpenExternalEditor, 80),
            AppCommand::OpenExternalEditor
        );
        failed.finish_external_edit(None);
        let AppCommand::Submit(submitted) = failed.handle_input(InputAction::Submit, 80) else {
            panic!("failed editor launch should restore the draft");
        };
        assert_eq!(submitted.images.len(), 1);

        let mut shell = model();
        let _ = shell.handle_input(InputAction::Insert("!echo before".to_string()), 80);
        assert_eq!(
            shell.handle_input(InputAction::OpenExternalEditor, 80),
            AppCommand::OpenExternalEditor
        );
        shell.finish_external_edit(Some("echo after".to_string()));
        assert!(shell.shell_mode());
        let AppCommand::RunShell(command) = shell.handle_input(InputAction::Submit, 80) else {
            panic!("external shell edit should stay in shell mode");
        };
        assert_eq!(command.as_str(), "echo after");
    }

    #[test]
    fn external_edit_is_idle_composer_only() {
        let mut overlay = model();
        overlay.set_command_catalog([command("model", "Switch model")]);
        let _ = overlay.handle_input(InputAction::Insert("/mo".to_string()), 80);
        assert_eq!(
            overlay.handle_input(InputAction::OpenExternalEditor, 80),
            AppCommand::None
        );
        assert!(overlay.external_edit_text().is_none());

        let mut dialog = model();
        dialog.require_workspace_trust("fixture");
        assert_eq!(
            dialog.handle_input(InputAction::OpenExternalEditor, 80),
            AppCommand::None
        );

        let mut busy = model();
        busy.begin_work();
        assert_eq!(
            busy.handle_input(InputAction::OpenExternalEditor, 80),
            AppCommand::None
        );
        assert!(busy.external_edit_text().is_none());
    }

    #[test]
    fn shell_mode_exits_before_work_interrupt_and_never_enters_prompt_history() {
        let mut app = model();
        app.seed_history(vec!["ordinary prompt".to_string()]);
        app.set_command_catalog([command("model", "Switch model")]);
        let _ = app.handle_input(
            InputAction::Insert("!echo @sample SHELL_SECRET".to_string()),
            80,
        );
        assert!(app.shell_mode());
        assert!(app.completion().is_none());

        app.begin_work();
        assert_eq!(app.handle_input(InputAction::Escape, 80), AppCommand::None);
        assert_eq!(app.editor.text(), "echo @sample SHELL_SECRET");
        assert_eq!(
            app.active_work(),
            WorkState::Busy {
                cancellation_requested: false
            }
        );
        assert_eq!(
            app.handle_input(InputAction::Escape, 80),
            AppCommand::CancelWork
        );

        app.apply_runtime(RuntimeUpdate::Stopped(StopState::Done));
        app.editor.replace_draft("");
        let _ = app.handle_input(InputAction::Insert("!echo SHELL_SECRET".to_string()), 80);
        let AppCommand::RunShell(command) = app.handle_input(InputAction::Submit, 80) else {
            panic!("a leading bang should submit one shell command");
        };
        assert_eq!(command.as_str(), "echo SHELL_SECRET");
        assert!(!format!("{command:?}").contains("SHELL_SECRET"));
        assert!(app.editor.text().is_empty());

        let _ = app.handle_input(InputAction::Insert("!".to_string()), 80);
        let _ = app.handle_input(InputAction::MoveUp, 80);
        assert_eq!(app.editor.text(), "echo SHELL_SECRET");
        let _ = app.handle_input(InputAction::MoveDown, 80);
        assert_eq!(app.editor.text(), "");
        let _ = app.handle_input(InputAction::Escape, 80);
        let _ = app.handle_input(InputAction::MoveUp, 80);
        assert_eq!(app.editor.text(), "ordinary prompt");
    }

    #[test]
    fn shell_submit_expands_text_paste_but_rejects_images() {
        let mut pasted = model();
        let _ = pasted.handle_input(InputAction::Insert("!echo ".to_string()), 80);
        let secret = format!("{}\nline 2\nline 3\nline 4", "PASTED_SHELL_SECRET");
        let _ = pasted.handle_input(InputAction::Paste(secret.clone()), 80);
        let AppCommand::RunShell(command) = pasted.handle_input(InputAction::Submit, 80) else {
            panic!("shell paste should submit");
        };
        assert_eq!(command.as_str(), format!("echo {secret}"));
        assert!(!format!("{command:?}").contains("PASTED_SHELL_SECRET"));

        let mut image = model();
        let _ = image.handle_input(InputAction::Insert("!echo image".to_string()), 80);
        assert!(image
            .attach_image("image/png", "IMAGE_SECRET", 128)
            .is_none());
    }

    #[test]
    fn shell_timeline_item_activates_and_finishes_in_place() {
        let mut app = model();
        let _ = app.handle_input(InputAction::Insert("!echo marker".to_string()), 80);
        let AppCommand::RunShell(command) = app.handle_input(InputAction::Submit, 80) else {
            panic!("shell command");
        };
        let item = app
            .append_shell(&command, false)
            .expect("shell timeline item");
        assert!(app.activate_shell(item));
        assert_eq!(
            app.active_timeline()
                .item(item)
                .expect("running shell")
                .activity,
            Some(ActivityState::Running)
        );
        let output = UserShellOutput::captured(0, "marker\n", "");
        assert!(app.finish_shell(item, &command, &output));
        let item = app.active_timeline().item(item).expect("finished shell");
        assert_eq!(item.kind, ItemKind::Shell);
        assert_eq!(item.text, "Shell echo marker 1 line\nmarker");
        assert_eq!(item.activity, Some(ActivityState::Success));
    }

    #[test]
    fn shell_timeline_summary_counts_stream_lines_and_encodes_failure_without_exit_copy() {
        let mut app = model();
        let _ = app.handle_input(InputAction::Insert("!failing-command".to_string()), 80);
        let AppCommand::RunShell(command) = app.handle_input(InputAction::Submit, 80) else {
            panic!("shell command");
        };
        let item = app.append_shell(&command, false).expect("shell item");
        let output = UserShellOutput::captured(5, "first\nsecond\n", "stderr only\n");
        assert!(app.finish_shell(item, &command, &output));
        let item = app.active_timeline().item(item).expect("finished shell");
        assert_eq!(item.kind, ItemKind::Shell);
        assert_eq!(
            item.text,
            "Shell failing-command 3 lines\nfirst\nsecond\nstderr only"
        );
        assert!(!item.text.contains("exit 5"));
        assert_eq!(item.activity, Some(ActivityState::Error));

        let mut empty = model();
        let _ = empty.handle_input(InputAction::Insert("!true".to_string()), 80);
        let AppCommand::RunShell(command) = empty.handle_input(InputAction::Submit, 80) else {
            panic!("empty shell command");
        };
        let item = empty
            .append_shell(&command, false)
            .expect("empty shell item");
        assert!(empty.finish_shell(item, &command, &UserShellOutput::captured(0, "", "")));
        assert_eq!(
            empty
                .active_timeline()
                .item(item)
                .expect("empty result")
                .text,
            "Shell true 0 lines"
        );
    }

    #[test]
    fn shell_mode_owns_up_down_history_and_blocks_prompt_reverse_search() {
        let mut app = model();
        app.seed_history(vec!["ordinary prompt".to_string()]);
        for command in ["!echo first", "!echo second"] {
            let _ = app.handle_input(InputAction::Insert(command.to_string()), 80);
            assert!(matches!(
                app.handle_input(InputAction::Submit, 80),
                AppCommand::RunShell(_)
            ));
        }
        let _ = app.handle_input(InputAction::Insert("!".to_string()), 80);
        assert_eq!(
            app.handle_input(InputAction::OpenReverseHistory, 80),
            AppCommand::None
        );
        assert!(!app.has_input_overlay());
        let _ = app.handle_input(InputAction::MoveUp, 80);
        assert_eq!(app.editor.text(), "echo second");
        let _ = app.handle_input(InputAction::MoveUp, 80);
        assert_eq!(app.editor.text(), "echo first");
        let _ = app.handle_input(InputAction::MoveDown, 80);
        assert_eq!(app.editor.text(), "echo second");
        let _ = app.handle_input(InputAction::MoveDown, 80);
        assert_eq!(app.editor.text(), "");
    }

    #[test]
    fn timeline_search_counts_messages_and_starts_at_the_newest() {
        let mut app = model();
        let older = app
            .active_timeline_mut()
            .push(ItemKind::User, "marker appears twice: marker")
            .expect("older");
        let newer = app
            .active_timeline_mut()
            .push(ItemKind::Assistant, "newer MARKER")
            .expect("newer");
        // `/search <query>` is emitted as an ordinary slash command; the host
        // opens timeline search seeded with the query.
        app.open_timeline_search("marker".to_string());

        assert_eq!(
            app.timeline_search(),
            Some(TimelineSearchView {
                query: "marker",
                current: 2,
                total: 2,
            })
        );
        let ViewportAnchor::Held(point) = app.active_timeline().viewport else {
            panic!("search should hold the selected match");
        };
        assert_eq!(point.item_id, newer);

        let _ = app.handle_input(InputAction::MoveUp, 80);
        assert_eq!(app.timeline_search().expect("search").current, 1);
        let ViewportAnchor::Held(point) = app.active_timeline().viewport else {
            panic!("search should hold the older match");
        };
        assert_eq!(point.item_id, older);

        // Target parity: Enter advances just like Down and leaves search open.
        assert_eq!(app.handle_input(InputAction::Submit, 80), AppCommand::None);
        assert_eq!(app.timeline_search().expect("search").current, 2);
        assert!(app.has_input_overlay());
    }

    #[test]
    fn ctrl_f_is_forward_character_with_a_draft_and_search_when_empty() {
        let mut app = model();
        app.editor.insert("ab");
        app.editor.move_left();
        assert_eq!(app.editor.cursor(), 1);
        let _ = app.handle_input(InputAction::ForwardCharOrSearch, 80);
        assert_eq!(app.editor.cursor(), 2);
        assert!(!app.has_input_overlay());

        app.editor.replace_draft("");
        let _ = app.handle_input(InputAction::ForwardCharOrSearch, 80);
        assert_eq!(
            app.timeline_search(),
            Some(TimelineSearchView {
                query: "",
                current: 0,
                total: 0,
            })
        );
    }

    #[test]
    fn timeline_search_escape_and_ctrl_c_restore_without_interrupt_or_exit_arm() {
        let payload = (1..=12)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut app = model();
        app.editor.insert_paste(payload.clone());
        app.begin_work();
        app.open_timeline_search("line".to_string());

        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::None
        );
        assert!(!app.exit_armed);
        assert_eq!(
            app.active_work(),
            WorkState::Busy {
                cancellation_requested: false
            }
        );
        let AppCommand::Submit(submitted) = app.handle_input(InputAction::Submit, 80) else {
            panic!("restored compact paste should submit");
        };
        assert_eq!(submitted.prompt, payload);
        assert_eq!(submitted.pastes.len(), 1);

        let mut command = model();
        command.begin_work();
        // The host opens timeline search from the emitted `/search` command.
        command.open_timeline_search("marker".to_string());
        assert_eq!(
            command.handle_input(InputAction::Escape, 80),
            AppCommand::None
        );
        assert_eq!(command.editor.text(), "");
        assert!(!command.exit_armed);
        assert_eq!(
            command.active_work(),
            WorkState::Busy {
                cancellation_requested: false
            }
        );
        assert_eq!(
            command.handle_input(InputAction::Escape, 80),
            AppCommand::CancelWork
        );
    }

    #[test]
    fn timeline_search_refresh_preserves_identity_then_uses_nearest_newer_match() {
        let mut app = model();
        let older = app
            .active_timeline_mut()
            .push(ItemKind::User, "targetx older")
            .expect("older");
        let middle = app
            .active_timeline_mut()
            .push(ItemKind::Assistant, "target middle")
            .expect("middle");
        let newer = app
            .active_timeline_mut()
            .push(ItemKind::Notice, "targetx newer")
            .expect("newer");
        app.open_timeline_search("target".to_string());
        let _ = app.handle_input(InputAction::MoveUp, 80);
        let selected = match app.active_timeline().viewport {
            ViewportAnchor::Held(point) => point.item_id,
            _ => panic!("held search result"),
        };
        assert_eq!(selected, middle);

        // The selected item no longer matches. Equidistant survivors prefer the
        // newer item so refresh does not jump unpredictably.
        let _ = app.handle_input(InputAction::Insert("x".to_string()), 80);
        let selected = match app.active_timeline().viewport {
            ViewportAnchor::Held(point) => point.item_id,
            _ => panic!("held fallback result"),
        };
        assert_eq!(selected, newer);
        assert_ne!(selected, older);

        // Appending a new matching item keeps the existing selected identity.
        app.apply_runtime(RuntimeUpdate::Warning("targetx newest".to_string()));
        let selected_after_stream = match app.active_timeline().viewport {
            ViewportAnchor::Held(point) => point.item_id,
            _ => panic!("held preserved result"),
        };
        assert_eq!(selected_after_stream, newer);
        assert_eq!(app.timeline_search().expect("search").total, 3);
    }

    #[test]
    fn timeline_search_fold_mapping_returns_original_character_boundaries() {
        let text = "zero İSTANBUL ẞtraße 東京";
        for (query, expected) in [
            ("i\u{307}stanbul", text.find('İ')),
            ("ßTRAßE", text.find('ẞ')),
            ("東京", text.find("東京")),
        ] {
            let byte = case_insensitive_match_byte(text, query).expect("Unicode match");
            assert!(
                text.is_char_boundary(byte),
                "{query} mapped inside a codepoint"
            );
            assert_eq!(Some(byte), expected);
        }
        assert_eq!(case_insensitive_match_byte(text, "東京"), text.find("東京"));
    }

    #[test]
    fn timeline_search_rejects_command_prefixes_and_no_match_navigation_is_inert() {
        let mut app = model();
        let _ = app.handle_input(InputAction::Insert("/searching".to_string()), 80);
        let AppCommand::RunSlash(submitted) = app.handle_input(InputAction::Submit, 80) else {
            panic!("non-search slash prefix should remain a slash command");
        };
        assert_eq!(submitted.prompt, "/searching");

        app.open_timeline_search("absent".to_string());
        assert_eq!(app.timeline_search().expect("search").current, 0);
        assert_eq!(app.timeline_search().expect("search").total, 0);
        let viewport = app.active_timeline().viewport;
        let _ = app.handle_input(InputAction::MoveUp, 80);
        let _ = app.handle_input(InputAction::MoveDown, 80);
        let _ = app.handle_input(InputAction::Submit, 80);
        assert_eq!(app.active_timeline().viewport, viewport);
        assert!(app.has_input_overlay());
    }

    #[test]
    fn prompt_submission_reanchors_and_pending_state_clears_in_place() {
        let mut app = model();
        let _ = app
            .active_timeline_mut()
            .push(ItemKind::Assistant, "old response");
        app.active_timeline_mut().scroll_by(-1, 20, 1);
        let prompt = app
            .append_prompt("queued", Some("12:34".to_string()), true)
            .expect("prompt");
        assert_eq!(
            app.active_timeline().viewport,
            crate::ViewportAnchor::FollowBottom
        );
        assert!(app.active_timeline().item(prompt).expect("prompt").pending);
        assert!(app.activate_prompt(prompt));
        assert!(!app.active_timeline().item(prompt).expect("prompt").pending);
        assert_eq!(
            app.active_timeline()
                .item(prompt)
                .expect("prompt")
                .trailing
                .as_deref(),
            Some("12:34")
        );
    }

    #[test]
    fn queued_prompts_stay_after_their_own_active_response() {
        let mut app = model();
        let active = app
            .append_prompt("active", Some("12:00".to_string()), false)
            .expect("active");
        app.begin_work();
        app.apply_runtime(RuntimeUpdate::Text("first ".to_string()));
        let queued_a = app
            .append_prompt("queued a", Some("12:01".to_string()), true)
            .expect("queued a");
        let queued_b = app
            .append_prompt("queued b", Some("12:02".to_string()), true)
            .expect("queued b");
        app.apply_runtime(RuntimeUpdate::Text("response".to_string()));
        app.apply_runtime(RuntimeUpdate::ToolStarted {
            id: "tool".to_string(),
            name: "inspect".to_string(),
            detail: String::new(),
        });

        let ids = app
            .active_timeline()
            .items()
            .iter()
            .map(|item| item.id)
            .collect::<Vec<_>>();
        assert_eq!(ids[0], active);
        assert_eq!(ids[ids.len() - 2..], [queued_a, queued_b]);
        assert_eq!(app.active_timeline().items()[1].text, "first response");

        app.apply_runtime(RuntimeUpdate::Stopped(StopState::Done));
        assert!(app.activate_prompt(queued_a));
        app.begin_work_before(Some(queued_b));
        app.apply_runtime(RuntimeUpdate::Text("answer a".to_string()));
        let texts = app
            .active_timeline()
            .items()
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            vec![
                "active",
                "first response",
                "inspect running",
                "queued a",
                "answer a",
                "queued b"
            ]
        );
    }

    #[test]
    fn tool_start_closes_the_active_assistant_and_reasoning_segments() {
        let mut app = model();
        let active = app
            .append_prompt("active", Some("12:00".to_string()), false)
            .expect("active");
        app.begin_work();

        app.apply_runtime(RuntimeUpdate::Text("assistant A".to_string()));
        app.apply_runtime(RuntimeUpdate::Reasoning("reasoning A".to_string()));
        // Both segments are open before the tool starts.
        assert!(app.active_projection().active_assistant.is_some());
        assert!(app.active_projection().active_reasoning.is_some());
        let assistant_a = app.active_timeline().items()[1].id;

        // A queued prompt must remain after the entire active turn, tool and all.
        let queued = app
            .append_prompt("queued", Some("12:01".to_string()), true)
            .expect("queued");

        app.apply_runtime(RuntimeUpdate::ToolStarted {
            id: "tool".to_string(),
            name: "inspect".to_string(),
            detail: String::new(),
        });
        // The tool boundary retires both open segments.
        assert!(app.active_projection().active_assistant.is_none());
        assert!(app.active_projection().active_reasoning.is_none());

        app.apply_runtime(RuntimeUpdate::Text("\nassistant B".to_string()));
        app.apply_runtime(RuntimeUpdate::Reasoning("\r\nreasoning B".to_string()));
        // Post-tool deltas open new segments, never the pre-tool ones.
        assert!(app.active_projection().active_assistant.is_some());
        assert_ne!(app.active_projection().active_assistant, Some(assistant_a));

        let items: Vec<(ItemKind, &str)> = app
            .active_timeline()
            .items()
            .iter()
            .map(|item| (item.kind, item.text.as_str()))
            .collect();
        assert_eq!(
            items,
            vec![
                (ItemKind::User, "active"),
                (ItemKind::Assistant, "assistant A"),
                (ItemKind::Reasoning, "reasoning A"),
                (ItemKind::Tool, "inspect running"),
                (ItemKind::Assistant, "assistant B"),
                (ItemKind::Reasoning, "reasoning B"),
                (ItemKind::User, "queued"),
            ]
        );
        // The active prompt leads and the queued prompt stays last: post-tool
        // segments land ahead of user work queued during the turn.
        let ids: Vec<_> = app
            .active_timeline()
            .items()
            .iter()
            .map(|item| item.id)
            .collect();
        assert_eq!(ids.first(), Some(&active));
        assert_eq!(ids.last(), Some(&queued));
    }

    #[test]
    fn a_report_body_is_absent_from_timeline_search() {
        let mut app = model();
        // The report body lives only in the takeover, never as a timeline item.
        app.open_report(
            "tree".to_string(),
            vec!["a body line containing SEARCHTOKEN inside".to_string()],
        );
        app.open_timeline_search("SEARCHTOKEN".to_string());
        // A search over the timeline finds no match — a hit with no row would leak.
        assert_eq!(app.timeline_search().map(|view| view.total), Some(0));
        assert!(!app
            .active_timeline()
            .items()
            .iter()
            .any(|item| item.text.contains("SEARCHTOKEN")));
    }

    #[test]
    fn timeline_search_skips_hidden_reasoning() {
        let mut timeline = Timeline::new();
        let _ = timeline.push(ItemKind::Assistant, "find MARKER here");
        let _ = timeline.push(ItemKind::Reasoning, "reasoning MARKER inside");
        // Visible: both items match.
        assert_eq!(timeline_search_matches(&timeline, "MARKER").len(), 2);
        // Hidden: only the assistant matches — a hidden reasoning item has no row,
        // so it must not surface as a search hit the UI cannot scroll to.
        timeline.set_reasoning_visible(false);
        let matches = timeline_search_matches(&timeline, "MARKER");
        assert_eq!(matches.len(), 1);
        assert_eq!(
            timeline.item(matches[0].item_id).map(|item| item.kind),
            Some(ItemKind::Assistant),
        );
    }

    #[test]
    fn toggling_reasoning_refreshes_an_open_search_to_drop_hidden_matches() {
        let mut app = model();
        // The only item that matches the query is a reasoning item.
        app.apply_runtime(RuntimeUpdate::Reasoning(
            "a unique NEEDLE token".to_string(),
        ));
        app.open_timeline_search("NEEDLE".to_string());
        assert_eq!(app.timeline_search().map(|view| view.total), Some(1));
        // Hiding reasoning refreshes the OPEN search — its only match had no row.
        app.toggle_reasoning();
        assert_eq!(app.timeline_search().map(|view| view.total), Some(0));
        // Showing again brings the match back.
        app.toggle_reasoning();
        assert_eq!(app.timeline_search().map(|view| view.total), Some(1));
    }

    #[test]
    fn hiding_reasoning_survives_a_conversation_clear() {
        let mut app = model();
        app.apply_runtime(RuntimeUpdate::Reasoning("thinking".to_string()));
        assert!(!app.toggle_reasoning(), "toggled to hidden");
        assert!(!app.reasoning_visible());
        // Clearing installs a fresh timeline; the hidden state must be reapplied.
        app.clear_conversation();
        assert!(!app.reasoning_visible());
        app.apply_runtime(RuntimeUpdate::Reasoning("more thinking".to_string()));
        assert!(!app.active_timeline().reasoning_visible());
        assert!(!app
            .active_timeline()
            .rows(80)
            .iter()
            .any(|row| row.text.contains("more thinking")));
    }

    #[test]
    fn dialog_content_is_sanitized_and_trust_is_explicit() {
        let mut app = model();
        app.require_workspace_trust("safe\u{1b}[2J-path");
        assert!(app.workspace_trust_pending());
        assert_eq!(
            app.dialog,
            Some(DialogState::Trust(TrustDialog {
                path: "safe-path".to_string(),
                selected: 0,
                path_selection: None,
            }))
        );
        assert!(!format!("{:?}", app.dialog).contains("safe-path"));
        app.clear_dialog();
        assert!(!app.workspace_trust_pending());

        app.require_workspace_trust("界".repeat(MAX_TOOL_DETAIL_BYTES));
        let bounded = app.trust().expect("trust view").path;
        assert!(bounded.len() <= MAX_TOOL_DETAIL_BYTES);
        assert!(bounded.is_char_boundary(bounded.len()));
        app.clear_dialog();

        app.request_approval("tool", "target\0", "write");
        assert_eq!(
            app.dialog,
            Some(DialogState::Approval {
                tool: "tool".to_string(),
                target: "target".to_string(),
                risk_class: "write".to_string(),
            })
        );
    }

    #[test]
    fn trust_choices_distinguish_session_remember_and_deny() {
        let mut app = model();
        app.require_workspace_trust("fixture");
        assert_eq!(app.trust().map(|trust| trust.selected), Some(0));
        assert_eq!(
            app.handle_trust_input(InputAction::Submit),
            TrustAction::ContinueSession
        );

        assert_eq!(
            app.handle_trust_input(InputAction::MoveDown),
            TrustAction::None
        );
        assert_eq!(app.trust().map(|trust| trust.selected), Some(1));
        assert_eq!(
            app.handle_trust_input(InputAction::Submit),
            TrustAction::Remember
        );

        app.select_trust_option(99);
        assert_eq!(app.trust().map(|trust| trust.selected), Some(2));
        assert_eq!(
            app.handle_trust_input(InputAction::Submit),
            TrustAction::Deny
        );
        assert_eq!(
            app.handle_trust_input(InputAction::Escape),
            TrustAction::Deny
        );
    }

    #[test]
    fn trust_path_selection_never_splits_a_grapheme() {
        let mut app = model();
        app.require_workspace_trust("fixture");
        let shown = "a\u{301}界".to_string();
        app.start_trust_path_selection(shown.clone(), 1);
        app.extend_trust_path_selection(&shown, "a\u{301}".len());

        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::Copy("a\u{301}".to_string())
        );
    }

    #[test]
    fn a_dialog_claims_focus_before_transient_overlays_and_ctrl_c() {
        let mut app = model();
        app.quick_help = true;
        app.open_theme_picker();
        let _ = app.handle_theme_picker_input(InputAction::MoveDown);
        assert_eq!(app.theme, Theme::Dim);

        app.begin_work();
        app.request_approval("write_file", "src/main.rs", "project write");
        assert!(!app.quick_help());
        assert!(!app.has_theme_picker());
        assert_eq!(app.theme, Theme::Default);
        assert_eq!(
            app.handle_input(InputAction::Insert("x".to_string()), 80),
            AppCommand::None
        );
        assert!(matches!(app.dialog, Some(DialogState::Approval { .. })));
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::CancelWork
        );
        assert!(matches!(app.dialog, Some(DialogState::Approval { .. })));
    }

    #[test]
    fn truthful_tab_configuration_preserves_order_and_removes_duplicates() {
        let mut app = model();
        assert_eq!(app.tabs, vec![TabId::Session, TabId::LocalMind]);
        assert_eq!(pair_model().tabs, vec![TabId::Session]);
        app.set_tabs([TabId::Activity, TabId::Session, TabId::Activity]);
        assert_eq!(app.tabs, vec![TabId::Activity, TabId::Session]);
        assert_eq!(app.active_tab, TabId::Session);

        app.set_tabs([]);
        assert_eq!(app.tabs, vec![TabId::Session]);
        assert_eq!(app.active_tab, TabId::Session);
    }

    #[test]
    fn completed_output_gains_semantic_styles_and_tools_remain_compact() {
        let mut app = model();
        app.begin_work();
        app.apply_runtime(RuntimeUpdate::Text(
            "# Result\nUse `cargo test` and [docs](https://example.test).".to_string(),
        ));
        app.apply_runtime(RuntimeUpdate::ToolStarted {
            id: "tool-1".to_string(),
            name: "inspect".to_string(),
            detail: "src/main.rs".to_string(),
        });
        app.apply_runtime(RuntimeUpdate::ToolFinished {
            id: "tool-1".to_string(),
            name: "inspect".to_string(),
            is_error: false,
            cancelled: false,
            output: "detail one\ndetail two".to_string(),
            duration_ms: 1_250,
        });
        app.apply_runtime(RuntimeUpdate::Stopped(StopState::Done));

        let assistant = &app.active_timeline().items()[0];
        assert!(assistant
            .styles
            .iter()
            .any(|span| span.style.role == SemanticRole::Heading));
        assert!(assistant
            .styles
            .iter()
            .any(|span| span.style.role == SemanticRole::Code));
        let tool = &app.active_timeline().items()[1];
        assert_eq!(tool.activity, Some(ActivityState::Success));
        assert!(!tool.expanded);
        assert_eq!(
            app.active_timeline().rows(80)[2].text,
            "inspect completed: src/main.rs · 2 lines · 1.2 s"
        );
        assert!(tool.text.contains("src/main.rs"));
        assert!(tool.text.contains("detail one\ndetail two"));
        assert!(!tool.text.contains("tool: inspect"));
        assert!(tool
            .styles
            .iter()
            .any(|span| { span.style.role == SemanticRole::Success && span.style.bold }));
        assert!(tool
            .styles
            .iter()
            .any(|span| span.style.role == SemanticRole::Muted));
    }

    #[test]
    fn cancelled_tool_has_a_distinct_terminal_state() {
        let mut app = model();
        app.apply_runtime(RuntimeUpdate::ToolStarted {
            id: "tool-1".to_string(),
            name: "run_shell".to_string(),
            detail: "cargo test".to_string(),
        });
        app.apply_runtime(RuntimeUpdate::ToolFinished {
            id: "tool-1".to_string(),
            name: "run_shell".to_string(),
            is_error: true,
            cancelled: true,
            output: "cancelled by the user; execution aborted".to_string(),
            duration_ms: 750,
        });

        let tool = &app.active_timeline().items()[0];
        assert_eq!(tool.activity, Some(ActivityState::Cancelled));
        assert_eq!(
            app.active_timeline().rows(80)[0].text,
            "Command cancelled: cargo test · 1 line · 750 ms"
        );
    }

    #[test]
    fn completed_tool_summary_counts_only_the_output_body() {
        let mut app = model();
        for (id, output, is_error, expected) in [
            (
                "empty",
                "tool: inspect\nstatus: success\noutput:\n",
                false,
                "inspect completed · 0 lines · 10 ms",
            ),
            (
                "one",
                "tool: inspect\nstatus: error\noutput:\nproblem",
                true,
                "inspect failed · 1 line · 10 ms",
            ),
            (
                "many",
                "tool: inspect\nstatus: success\noutput:\nfirst\nsecond\nthird",
                false,
                "inspect completed · 3 lines · 10 ms",
            ),
        ] {
            app.apply_runtime(RuntimeUpdate::ToolFinished {
                id: id.to_string(),
                name: "inspect".to_string(),
                is_error,
                cancelled: false,
                output: output.to_string(),
                duration_ms: 10,
            });
            let tool = app.active_timeline().items().last().expect("tool item");
            assert_eq!(tool.text.lines().next(), Some(expected));
            assert!(!tool.text.contains("tool: inspect"));
            assert!(!tool.text.contains("status:"));
        }
    }

    #[test]
    fn tool_headlines_name_known_targets_and_keep_unknown_tools_neutral() {
        let mut app = model();
        app.apply_runtime(RuntimeUpdate::ToolStarted {
            id: "read".to_string(),
            name: "read_file".to_string(),
            detail: "src/lib.rs\nignored".to_string(),
        });
        app.apply_runtime(RuntimeUpdate::ToolStarted {
            id: "remote".to_string(),
            name: "mcp__docs_lookup".to_string(),
            detail: "widget API".to_string(),
        });

        assert_eq!(
            app.active_timeline().items()[0].text,
            "Reading src/lib.rs ignored"
        );
        assert_eq!(
            app.active_timeline().items()[1].text,
            "mcp__docs_lookup running: widget API"
        );
        assert!(app.active_timeline().items().iter().all(|item| {
            item.styles
                .iter()
                .any(|span| span.style.role == SemanticRole::Tool && span.style.bold)
        }));
    }

    #[test]
    fn recognizable_unified_diffs_gain_truthful_deltas_and_line_styles() {
        let mut app = model();
        app.apply_runtime(RuntimeUpdate::ToolStarted {
            id: "edit".to_string(),
            name: "edit_file".to_string(),
            detail: "src/lib.rs".to_string(),
        });
        app.apply_runtime(RuntimeUpdate::ToolFinished {
            id: "edit".to_string(),
            name: "edit_file".to_string(),
            is_error: false,
            cancelled: false,
            output: concat!(
                "tool: edit_file\nstatus: success\noutput:\n",
                "--- a/src/lib.rs\n",
                "+++ b/src/lib.rs\n",
                "@@ -1,2 +1,3 @@\n",
                "-old\n",
                "+new\n",
                "+more\n",
                " context"
            )
            .to_string(),
            duration_ms: 20,
        });

        let tool = &app.active_timeline().items()[0];
        assert_eq!(
            tool.text.lines().next(),
            Some("Edited src/lib.rs · +2/-1 · 7 lines · 20 ms")
        );
        assert!(!tool.text.contains("tool: edit_file"));
        assert!(tool
            .styles
            .iter()
            .any(|span| span.style.role == SemanticRole::Success));
        assert!(tool
            .styles
            .iter()
            .any(|span| span.style.role == SemanticRole::Error));

        let mut prose = model();
        prose.apply_runtime(RuntimeUpdate::ToolFinished {
            id: "plain".to_string(),
            name: "edit_file".to_string(),
            is_error: false,
            cancelled: false,
            output: "+ added in prose\n- removed in prose".to_string(),
            duration_ms: 20,
        });
        let plain = &prose.active_timeline().items()[0];
        assert_eq!(plain.text.lines().next(), Some("Edited · 2 lines · 20 ms"));
        assert_eq!(
            plain
                .styles
                .iter()
                .filter(|span| matches!(
                    span.style.role,
                    SemanticRole::Success | SemanticRole::Error
                ))
                .count(),
            1,
            "only the successful headline is semantic; prose signs are muted"
        );
    }

    #[test]
    fn searching_retained_tool_output_expands_a_hidden_match() {
        let mut app = model();
        app.apply_runtime(RuntimeUpdate::ToolFinished {
            id: "long".to_string(),
            name: "inspect".to_string(),
            is_error: false,
            cancelled: false,
            output: "one\ntwo\nthree\nfour\nneedle after preview".to_string(),
            duration_ms: 25,
        });
        let id = app.active_timeline().items()[0].id;
        assert!(!app.active_timeline().item(id).expect("tool").expanded);
        assert_eq!(app.active_timeline().rows(80).len(), 4);

        app.open_timeline_search("needle after preview".to_string());

        assert_eq!(app.timeline_search().map(|search| search.total), Some(1));
        assert!(app.active_timeline().item(id).expect("tool").expanded);
        assert!(app
            .active_timeline()
            .rows(80)
            .iter()
            .any(|row| row.text == "needle after preview"));
    }

    #[test]
    fn question_dialog_owns_choices_other_text_and_cancellation() {
        let mut app = model();
        app.request_question(
            Some("Palette".to_string()),
            "Pick a color",
            [
                QuestionOption {
                    label: "Red".to_string(),
                    description: None,
                },
                QuestionOption {
                    label: "Blue".to_string(),
                    description: Some("cool tone".to_string()),
                },
            ],
            false,
            1,
            1,
        );
        assert_eq!(
            app.handle_question_input(InputAction::MoveDown),
            QuestionAction::None
        );
        assert_eq!(
            app.handle_question_input(InputAction::Submit),
            QuestionAction::Submit(QuestionResponse::Selected(vec!["Blue".to_string()]))
        );

        app.select_question_option(2);
        assert_eq!(
            app.handle_question_input(InputAction::Submit),
            QuestionAction::None
        );
        let _ = app.handle_question_input(InputAction::Insert("Cyan界".to_string()));
        let _ = app.handle_question_input(InputAction::MoveLeft);
        let _ = app.handle_question_input(InputAction::Backspace);
        let submitted = app.handle_question_input(InputAction::Submit);
        assert_eq!(
            submitted,
            QuestionAction::Submit(QuestionResponse::Other("Cya界".to_string()))
        );
        assert!(!format!("{submitted:?}").contains("Cya"));
        let _ = app.handle_question_input(InputAction::Escape);
        assert_eq!(
            app.handle_question_input(InputAction::Escape),
            QuestionAction::Cancel
        );
        let debug = format!("{:?}", app.dialog);
        assert!(!debug.contains("Pick a color"));
        assert!(!debug.contains("Cya"));
    }

    #[test]
    fn question_other_keeps_the_complete_long_answer_and_moves_to_both_ends() {
        let mut app = model();
        app.request_question(None, "Explain", Vec::<QuestionOption>::new(), false, 1, 1);
        assert_eq!(
            app.handle_question_input(InputAction::Submit),
            QuestionAction::None
        );
        let pasted = format!("START {} END", "context ".repeat(120));
        let _ = app.handle_question_input(InputAction::Paste(pasted.clone()));
        let question = app.question().expect("question");
        assert_eq!(question.other, pasted);
        assert_eq!(question.other_cursor, pasted.len());

        let _ = app.handle_question_input(InputAction::MoveTextStart);
        assert_eq!(app.question().expect("question").other_cursor, 0);
        let _ = app.handle_question_input(InputAction::MoveTextEnd);
        assert_eq!(app.question().expect("question").other_cursor, pasted.len());
        assert_eq!(
            app.handle_question_input(InputAction::Submit),
            QuestionAction::Submit(QuestionResponse::Other(pasted))
        );
    }

    #[test]
    fn multi_select_question_toggles_and_returns_every_checked_label_in_order() {
        let mut app = model();
        app.request_question(
            None,
            "Choose stores",
            [
                QuestionOption {
                    label: "SQLite".to_string(),
                    description: None,
                },
                QuestionOption {
                    label: "Postgres".to_string(),
                    description: None,
                },
            ],
            true,
            1,
            1,
        );
        assert_eq!(
            app.handle_question_input(InputAction::Insert(" ".to_string())),
            QuestionAction::None
        );
        let _ = app.handle_question_input(InputAction::MoveDown);
        let _ = app.handle_question_input(InputAction::Insert(" ".to_string()));
        assert_eq!(
            app.handle_question_input(InputAction::Submit),
            QuestionAction::Submit(QuestionResponse::Selected(vec![
                "SQLite".to_string(),
                "Postgres".to_string(),
            ]))
        );
    }

    #[test]
    fn ask_user_runtime_row_resolves_in_place_with_the_selected_answer() {
        let mut app = model();
        app.apply_runtime(RuntimeUpdate::ToolStarted {
            id: "ask-1".to_string(),
            name: "ask_user".to_string(),
            detail: "Do you prefer Red or Blue?".to_string(),
        });
        assert_eq!(app.active_timeline().items()[0].kind, ItemKind::Question);
        assert_eq!(
            app.active_timeline().items()[0].text,
            "Asking user Do you prefer Red or Blue?"
        );
        app.apply_runtime(RuntimeUpdate::ToolFinished {
            id: "ask-1".to_string(),
            name: "ask_user".to_string(),
            is_error: false,
            cancelled: false,
            output: "tool: ask_user\nstatus: success\noutput:\nUser selected: Blue".to_string(),
            duration_ms: 100,
        });
        assert_eq!(app.active_timeline().items().len(), 1);
        assert_eq!(
            app.active_timeline().items()[0].text,
            "Asked user Do you prefer Red or Blue?\nUser selected: Blue"
        );
        assert_eq!(
            app.active_timeline().items()[0].activity,
            Some(ActivityState::Success)
        );
    }

    #[test]
    fn tool_detail_and_output_bounds_preserve_unicode_head_and_tail() {
        let detail = format!("{}界", "x".repeat(MAX_TOOL_DETAIL_BYTES));
        let bounded_detail = bounded_inline_text(&detail, MAX_TOOL_DETAIL_BYTES);
        assert!(bounded_detail.len() <= MAX_TOOL_DETAIL_BYTES);
        assert!(bounded_detail.ends_with('…'));

        let output = format!(
            "HEAD界{}TAIL界",
            "x".repeat(MAX_TOOL_OUTPUT_BYTES.saturating_add(100))
        );
        let bounded = bounded_view_text(&output, MAX_TOOL_OUTPUT_BYTES);
        assert!(bounded.len() <= MAX_TOOL_OUTPUT_BYTES);
        assert!(bounded.starts_with("HEAD界"));
        assert!(bounded.ends_with("TAIL界"));
        assert!(bounded.contains("middle omitted from terminal view"));

        let lines = MAX_TOOL_OUTPUT_BYTES / 2 + 100;
        let mut app = model();
        app.apply_runtime(RuntimeUpdate::ToolFinished {
            id: "large".to_string(),
            name: "inspect".to_string(),
            is_error: false,
            cancelled: false,
            output: "x\n".repeat(lines),
            duration_ms: 1,
        });
        let tool = &app.active_timeline().items()[0];
        assert!(tool
            .text
            .lines()
            .next()
            .is_some_and(|headline| headline.contains(&format!("{lines} lines"))));
        assert!(tool.text.contains("terminal view truncated"));
        assert!(tool.text.contains("middle omitted from terminal view"));
        let presentation = tool.tool.expect("bounded tool presentation");
        assert_eq!(presentation.source_lines, lines);
        assert_eq!(
            presentation.source_bytes,
            lines.saturating_mul(2).saturating_sub(1)
        );
        assert!(presentation.retained_lines < presentation.source_lines);
        assert_eq!(
            presentation.retained_bytes,
            tool.text[presentation.metadata_end + 1..].len()
        );
        assert!(presentation.retained_bytes <= MAX_TOOL_OUTPUT_BYTES);
        assert!(presentation.terminal_truncated);
        assert!(presentation.metadata_start < presentation.metadata_end);
    }

    #[test]
    fn work_activity_tracks_only_the_current_operation() {
        let mut app = model();
        assert!(app.active_work_activity().is_none());

        app.begin_work_with_label("Compacting");
        let (label, elapsed) = app.active_work_activity().expect("active operation");
        assert_eq!(label, "Compacting");
        assert!(elapsed < Duration::from_secs(1));

        app.clear_cancellation_request();
        assert_eq!(
            app.active_work_activity().map(|(label, _)| label),
            Some("Compacting")
        );

        app.apply_runtime(RuntimeUpdate::Stopped(StopState::Done));
        assert!(app.active_work_activity().is_none());
    }

    fn review_row(state: &str, requires_edit: bool, promoted: bool) -> LocalMindReviewRow {
        LocalMindReviewRow {
            id: "candidate-1".to_string(),
            state: state.to_string(),
            session_id: "session-1".to_string(),
            summary: "Prefer bounded terminal views".to_string(),
            category: "workflow".to_string(),
            confidence: "92%".to_string(),
            note: None,
            replacement: None,
            seen_count: 1,
            evidence: Some("A large report stayed responsive.".to_string()),
            requires_edit,
            promoted,
        }
    }

    fn localmind_data(review: Vec<LocalMindReviewRow>) -> LocalMindData {
        LocalMindData {
            docs: vec!["guide.md · 3 chunks".to_string()],
            graph: vec!["12 files · 44 symbols".to_string()],
            memory: vec!["memory-1 · workflow".to_string()],
            review,
            skills: vec!["pending · terminal-helper".to_string()],
            audit: vec!["accepted · candidate-1".to_string()],
        }
    }

    #[test]
    fn localmind_cycles_six_sections_without_leaving_its_active_tab() {
        let mut app = model();
        app.open_localmind(localmind_data(Vec::new()));
        assert_eq!(app.active_tab, TabId::LocalMind);
        assert_eq!(app.active_body(), ActiveBody::LocalMind);
        assert!(!app.has_takeover());
        assert_eq!(app.localmind_section(), Some(LocalMindSection::Docs));
        for expected in [
            LocalMindSection::Graph,
            LocalMindSection::Memory,
            LocalMindSection::Review,
            LocalMindSection::Skills,
            LocalMindSection::Audit,
            LocalMindSection::Docs,
        ] {
            assert_eq!(
                app.handle_input(InputAction::AcceptCompletion, 80),
                AppCommand::None
            );
            assert_eq!(app.localmind_section(), Some(expected));
            assert_eq!(app.active_body(), ActiveBody::LocalMind);
        }
        assert_eq!(
            app.handle_input(InputAction::PreviousLocalMindSection, 80),
            AppCommand::None
        );
        assert_eq!(app.localmind_section(), Some(LocalMindSection::Audit));
        assert_eq!(app.handle_input(InputAction::Escape, 80), AppCommand::None);
        assert_eq!(app.active_tab, TabId::Session);
        assert_eq!(app.active_body(), ActiveBody::Session);
        assert_eq!(app.localmind_section(), Some(LocalMindSection::Audit));
        assert!(!app.has_takeover());
    }

    #[test]
    fn localmind_tab_state_survives_session_and_two_level_overlay_escape() {
        let mut app = model();
        app.open_localmind(localmind_data(Vec::new()));
        let _ = app.handle_input(InputAction::AcceptCompletion, 80);
        assert_eq!(app.localmind_section(), Some(LocalMindSection::Graph));
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::Copy("12 files · 44 symbols".to_string())
        );
        assert_eq!(app.active_body(), ActiveBody::LocalMind);

        app.open_help();
        assert_eq!(app.active_body(), ActiveBody::Takeover);
        assert_eq!(app.active_tab, TabId::LocalMind);
        assert_eq!(app.handle_input(InputAction::Escape, 80), AppCommand::None);
        assert_eq!(app.active_body(), ActiveBody::LocalMind);
        assert_eq!(app.localmind_section(), Some(LocalMindSection::Graph));

        assert_eq!(app.handle_input(InputAction::Escape, 80), AppCommand::None);
        assert_eq!(app.active_body(), ActiveBody::Session);
        assert_eq!(app.localmind_section(), Some(LocalMindSection::Graph));
        assert_eq!(
            app.handle_input(InputAction::Insert("session draft".to_string()), 80),
            AppCommand::None
        );
        assert_eq!(app.editor.text(), "session draft");

        assert!(app.activate_tab(TabId::LocalMind));
        assert_eq!(app.active_body(), ActiveBody::LocalMind);
        assert_eq!(app.localmind_section(), Some(LocalMindSection::Graph));
    }

    #[test]
    fn review_actions_require_identity_and_obey_candidate_state() {
        let mut app = model();
        app.open_localmind(localmind_data(vec![review_row("Pending", false, false)]));
        for _ in 0..3 {
            let _ = app.handle_input(InputAction::AcceptCompletion, 80);
        }

        assert_eq!(
            app.handle_input(InputAction::Insert("a".to_string()), 80),
            AppCommand::None,
            "an unnamed reviewer cannot emit a write intent"
        );
        let _ = app.handle_input(InputAction::Insert("Ada".to_string()), 80);
        let _ = app.handle_input(InputAction::Submit, 80);
        let command = app.handle_input(InputAction::Insert("a".to_string()), 80);
        assert_eq!(
            command,
            AppCommand::LocalMindReview(LocalMindReviewIntent {
                candidate_id: "candidate-1".to_string(),
                reviewer: "Ada".to_string(),
                action: LocalMindReviewAction::Accept,
            })
        );

        app.refresh_localmind(localmind_data(vec![review_row("Accepted", false, false)]));
        assert_eq!(app.localmind_section(), Some(LocalMindSection::Review));
        assert_eq!(app.localmind_reviewer(), Some("Ada"));
        assert!(matches!(
            app.handle_input(InputAction::Insert("p".to_string()), 80),
            AppCommand::LocalMindReview(LocalMindReviewIntent {
                action: LocalMindReviewAction::Promote,
                ..
            })
        ));

        app.refresh_localmind(localmind_data(vec![review_row("Pending", true, false)]));
        assert_eq!(
            app.handle_input(InputAction::Insert("a".to_string()), 80),
            AppCommand::None,
            "source excerpts requiring edit cannot be accepted as standalone lessons"
        );

        app.refresh_localmind(localmind_data(vec![review_row("Accepted", true, false)]));
        assert_eq!(
            app.handle_input(InputAction::Insert("p".to_string()), 80),
            AppCommand::None,
            "an accepted source excerpt still requires a standalone edit before promotion"
        );
        app.refresh_localmind(localmind_data(vec![review_row("Edited", true, false)]));
        assert!(matches!(
            app.handle_input(InputAction::Insert("p".to_string()), 80),
            AppCommand::LocalMindReview(LocalMindReviewIntent {
                action: LocalMindReviewAction::Promote,
                ..
            })
        ));
    }

    #[test]
    fn reviewer_identity_cap_never_splits_a_grapheme() {
        let mut app = model();
        app.open_localmind(localmind_data(vec![review_row("Pending", false, false)]));
        for _ in 0..3 {
            let _ = app.handle_input(InputAction::AcceptCompletion, 80);
        }
        let _ = app.handle_input(InputAction::Insert("a".to_string()), 80);
        let _ = app.handle_input(InputAction::Insert("x".repeat(127)), 80);
        let _ = app.handle_input(InputAction::Insert("é".to_string()), 80);
        assert_eq!(app.localmind_reviewer().expect("reviewer").len(), 127);

        let _ = app.handle_input(InputAction::Backspace, 80);
        let _ = app.handle_input(InputAction::Insert("é".to_string()), 80);
        let reviewer = app.localmind_reviewer().expect("reviewer");
        assert_eq!(reviewer.len(), MAX_REVIEWER_BYTES);
        assert!(reviewer.ends_with('é'));
    }

    #[test]
    fn localmind_defensively_caps_injected_rows() {
        let mut app = model();
        app.open_localmind(LocalMindData {
            docs: (0..10_000).map(|index| format!("doc-{index}")).collect(),
            ..LocalMindData::default()
        });
        let view = app
            .localmind_tab()
            .and_then(|view| view.localmind)
            .expect("localmind view");
        assert_eq!(view.lines.len(), MAX_LOCALMIND_VIEW_ROWS);
        assert!(view
            .lines
            .last()
            .is_some_and(|line| line.contains("more rows omitted")));
    }
}
