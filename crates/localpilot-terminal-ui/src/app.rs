use std::collections::BTreeMap;
use std::fmt;
use unicode_segmentation::UnicodeSegmentation;

use crate::editor::{EditorSnapshot, EditorToken, SubmittedInput};
use crate::presentation::semantic_ranges;
use crate::{
    sanitize_text, ActivityState, ContentPoint, Editor, ItemId, ItemKind, SemanticRole,
    StyledRange, TextStyle, Theme, Timeline,
};

const MAX_TOOL_DETAIL_BYTES: usize = 4 * 1024;
const MAX_TOOL_OUTPUT_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TabId {
    Session,
    Plan,
    Activity,
    Settings,
}

impl TabId {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Session => "Session",
            Self::Plan => "Plan",
            Self::Activity => "Activity",
            Self::Settings => "Settings",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub version: String,
    pub provider: String,
    pub model: String,
    pub workspace: String,
    pub branch: Option<String>,
    pub workspace_dirty: Option<bool>,
    pub mode: String,
    pub profile: String,
    pub session_id: String,
    pub session_name: Option<String>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkState {
    Idle,
    Busy { cancellation_requested: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    CancelOrExit,
    Escape,
    OpenReverseHistory,
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
    Submit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InputOverlay {
    ReverseHistory(ReverseHistoryState),
    Completion(CompletionState),
    TimelineSearch(TimelineSearchState),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakeoverKind {
    Diff,
    Help,
    Sessions,
    Settings,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct TakeoverState {
    kind: TakeoverKind,
    scroll: usize,
    file_scroll: usize,
    selected: usize,
    settings: Vec<SettingEntry>,
    sessions: Vec<SessionEntry>,
    diff_files: Vec<DiffFile>,
    diff_pane: DiffPane,
    selected_file: usize,
    tree_visible: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TakeoverView<'a> {
    pub kind: TakeoverKind,
    pub scroll: usize,
    pub file_scroll: usize,
    pub selected: usize,
    pub commands: &'a [CompletionCommand],
    pub settings: &'a [SettingEntry],
    pub sessions: &'a [SessionEntry],
    pub diff_files: &'a [DiffFile],
    pub diff_pane: DiffPane,
    pub selected_file: usize,
    pub tree_visible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingEntry {
    pub section: String,
    pub name: String,
    pub value: String,
    pub description: String,
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

fn sanitize_inline(text: &str) -> String {
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
struct ThemePickerState {
    original: Theme,
    selected: usize,
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct TimelineSearchState {
    query: String,
    matches: Vec<ContentPoint>,
    selected: Option<usize>,
    original_draft: EditorSnapshot,
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
    question: String,
    options: Vec<String>,
    selected: usize,
    editing_other: bool,
    other: String,
    other_cursor: usize,
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
            .field("editing_other", &self.editing_other)
            .field(
                "other",
                &format_args!("<{} bytes redacted>", self.other.len()),
            )
            .field("other_cursor", &self.other_cursor)
            .finish()
    }
}

#[derive(Clone, Copy)]
pub(crate) struct QuestionView<'a> {
    pub question: &'a str,
    pub options: &'a [String],
    pub selected: usize,
    pub editing_other: bool,
    pub other: &'a str,
    pub other_cursor: usize,
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
            .field("editing_other", &self.editing_other)
            .field(
                "other",
                &format_args!("<{} bytes redacted>", self.other.len()),
            )
            .field("other_cursor", &self.other_cursor)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum QuestionAction {
    None,
    Submit(String),
    Cancel,
}

impl fmt::Debug for QuestionAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => formatter.write_str("None"),
            Self::Submit(answer) => formatter
                .debug_tuple("Submit")
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
    Stopped(StopState),
}

#[derive(Clone, PartialEq, Eq)]
struct ActiveTool {
    item_id: ItemId,
    detail: String,
}

impl fmt::Debug for ActiveTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ActiveTool")
            .field("item_id", &self.item_id)
            .field(
                "detail",
                &format_args!("<{} bytes redacted>", self.detail.len()),
            )
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct AppModel {
    pub header: Header,
    pub capabilities: TerminalCapabilities,
    pub theme: Theme,
    pub tabs: Vec<TabId>,
    pub active_tab: TabId,
    pub timeline: Timeline,
    pub editor: Editor,
    pub focus: Focus,
    pub work: WorkState,
    pub exit_armed: bool,
    pub exit_requested: bool,
    print_transcript_on_exit: bool,
    pub plan: Vec<PlanEntry>,
    pub usage: Option<(u64, u64)>,
    pub context_usage: Option<(usize, usize)>,
    pub stream_bytes: usize,
    copy_on_select: bool,
    pub dialog: Option<DialogState>,
    takeover: Option<TakeoverState>,
    theme_picker: Option<ThemePickerState>,
    quick_help: bool,
    input_overlay: Option<InputOverlay>,
    external_edit_snapshot: Option<EditorSnapshot>,
    command_catalog: Vec<CompletionCommand>,
    command_values: BTreeMap<String, Vec<CompletionItem>>,
    workspace_files: Vec<String>,
    workspace_files_ready: bool,
    active_assistant: Option<ItemId>,
    active_reasoning: Option<ItemId>,
    active_tools: BTreeMap<String, ActiveTool>,
    active_insert_before: Option<ItemId>,
}

impl AppModel {
    #[must_use]
    pub fn new(header: Header, capabilities: TerminalCapabilities) -> Self {
        let header = Header {
            version: sanitize_text(&header.version),
            provider: sanitize_text(&header.provider),
            model: sanitize_text(&header.model),
            workspace: sanitize_text(&header.workspace),
            branch: header.branch.map(|branch| sanitize_text(&branch)),
            workspace_dirty: header.workspace_dirty,
            mode: sanitize_text(&header.mode),
            profile: sanitize_text(&header.profile),
            session_id: sanitize_inline(&header.session_id),
            session_name: header.session_name.map(|name| sanitize_inline(&name)),
        };
        Self {
            header,
            capabilities,
            theme: Theme::Default,
            tabs: vec![TabId::Session],
            active_tab: TabId::Session,
            timeline: Timeline::new(),
            editor: Editor::default(),
            focus: Focus::Composer,
            work: WorkState::Idle,
            exit_armed: false,
            exit_requested: false,
            print_transcript_on_exit: false,
            plan: Vec::new(),
            usage: None,
            context_usage: None,
            stream_bytes: 0,
            copy_on_select: false,
            dialog: None,
            takeover: None,
            theme_picker: None,
            quick_help: false,
            input_overlay: None,
            external_edit_snapshot: None,
            command_catalog: Vec::new(),
            command_values: BTreeMap::new(),
            workspace_files: Vec::new(),
            workspace_files_ready: false,
            active_assistant: None,
            active_reasoning: None,
            active_tools: BTreeMap::new(),
            active_insert_before: None,
        }
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

    pub fn handle_input(&mut self, action: InputAction, editor_width: u16) -> AppCommand {
        if self.dialog.is_some() {
            if !matches!(action, InputAction::CancelOrExit) {
                self.exit_armed = false;
                return AppCommand::None;
            }
            return self.cancel_or_exit();
        }
        if matches!(action, InputAction::CancelOrExit) && self.theme_picker.is_some() {
            self.exit_armed = false;
            self.close_theme_picker(true);
            return AppCommand::None;
        }
        if matches!(action, InputAction::CancelOrExit)
            && self.quick_help
            && self.timeline.selected_text().is_none()
        {
            self.exit_armed = false;
            self.quick_help = false;
            return AppCommand::None;
        }
        if matches!(action, InputAction::CancelOrExit) && self.takeover.is_some() {
            self.exit_armed = false;
            self.takeover = None;
            return AppCommand::None;
        }
        if matches!(action, InputAction::CancelOrExit) && self.input_overlay.is_some() {
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
        if self.input_overlay.is_some() {
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
            self.editor.exit_shell_mode();
            return AppCommand::None;
        }
        match action {
            InputAction::CancelOrExit => AppCommand::None,
            InputAction::Escape => self.interrupt_work(),
            InputAction::OpenReverseHistory
                if self.focus == Focus::Composer && !self.editor.is_shell_mode() =>
            {
                self.open_reverse_history();
                AppCommand::None
            }
            InputAction::NavigateTimeline(navigation) => AppCommand::NavigateTimeline(navigation),
            InputAction::Insert(text) if self.focus == Focus::Composer => {
                if text == "?"
                    && self.editor.text().is_empty()
                    && !self.editor.is_shell_mode()
                    && self.work == WorkState::Idle
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
                if self.focus == Focus::Composer && self.work == WorkState::Idle =>
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
                } else if self.open_theme_for_draft()
                    || self.open_help_for_draft()
                    || self.open_command_values_for_draft()
                {
                    AppCommand::None
                } else if let Some(query) = timeline_search_command(self.editor.text()) {
                    // The slash command is an invocation, not a draft to restore
                    // when search closes.
                    self.editor.replace_draft(String::new());
                    self.open_timeline_search(query);
                    AppCommand::None
                } else {
                    self.submit_editor()
                }
            }
            InputAction::Insert(_)
            | InputAction::Paste(_)
            | InputAction::OpenReverseHistory
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
            | InputAction::Submit => AppCommand::None,
        }
    }

    pub fn apply_runtime(&mut self, update: RuntimeUpdate) {
        match update {
            RuntimeUpdate::Text(text) => {
                self.stream_bytes = self.stream_bytes.saturating_add(text.len());
                if !self.append_active(self.active_assistant, &text) {
                    self.active_assistant = self.push_runtime_item(ItemKind::Assistant, text);
                }
            }
            RuntimeUpdate::Reasoning(text) => {
                self.stream_bytes = self.stream_bytes.saturating_add(text.len());
                if !self.append_active(self.active_reasoning, &text) {
                    self.active_reasoning = self.push_runtime_item(ItemKind::Reasoning, text);
                }
            }
            RuntimeUpdate::ToolStarted { id, name, detail } => {
                let detail = bounded_inline_text(&detail, MAX_TOOL_DETAIL_BYTES);
                let question = name == "ask_user";
                let mut text = if question {
                    format!("Asking user {detail}")
                } else {
                    sanitize_inline(&name)
                };
                if !question && !detail.is_empty() {
                    text.push('\n');
                    text.push_str(&detail);
                }
                let kind = if question {
                    ItemKind::Question
                } else {
                    ItemKind::Tool
                };
                if let Some(item) = self.push_runtime_item(kind, text) {
                    let _ = self
                        .timeline
                        .set_activity(item, Some(ActivityState::Running));
                    self.active_tools.insert(
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
                    if let Some(active) = self.active_tools.remove(&id) {
                        let mut text = format!("Asked user {}", active.detail);
                        if !output.is_empty() {
                            text.push('\n');
                            text.push_str(&output);
                        }
                        let _ = self.timeline.replace_text(active.item_id, text);
                        let activity = if cancelled {
                            ActivityState::Cancelled
                        } else if is_error {
                            ActivityState::Error
                        } else {
                            ActivityState::Success
                        };
                        let _ = self.timeline.set_activity(active.item_id, Some(activity));
                        self.style_activity(active.item_id, activity);
                    } else {
                        let mut text = "Asked user".to_string();
                        if !output.is_empty() {
                            text.push('\n');
                            text.push_str(&output);
                        }
                        let _ = self.push_runtime_item(ItemKind::Question, text);
                    }
                    if matches!(self.input_overlay, Some(InputOverlay::TimelineSearch(_))) {
                        self.refresh_timeline_search();
                    }
                    return;
                }
                let state = if cancelled {
                    "cancelled"
                } else if is_error {
                    "failed"
                } else {
                    "completed"
                };
                let mut text = format!(
                    "{} {state} · {}",
                    sanitize_inline(&name),
                    format_tool_duration(duration_ms)
                );
                let output = bounded_view_text(&output, MAX_TOOL_OUTPUT_BYTES);
                if let Some(active) = self.active_tools.remove(&id) {
                    if !active.detail.is_empty() {
                        text.push('\n');
                        text.push_str(&active.detail);
                    }
                    if !output.is_empty() {
                        text.push('\n');
                        text.push_str(&output);
                    }
                    let _ = self.timeline.replace_text(active.item_id, text);
                    let activity = if cancelled {
                        ActivityState::Cancelled
                    } else if is_error {
                        ActivityState::Error
                    } else {
                        ActivityState::Success
                    };
                    let _ = self.timeline.set_activity(active.item_id, Some(activity));
                    self.style_activity(active.item_id, activity);
                } else if let Some(item) = self.push_runtime_item(ItemKind::Tool, {
                    if !output.is_empty() {
                        text.push('\n');
                        text.push_str(&output);
                    }
                    text
                }) {
                    let activity = if cancelled {
                        ActivityState::Cancelled
                    } else if is_error {
                        ActivityState::Error
                    } else {
                        ActivityState::Success
                    };
                    let _ = self.timeline.set_activity(item, Some(activity));
                    self.style_activity(item, activity);
                }
            }
            RuntimeUpdate::Usage {
                input_tokens,
                output_tokens,
            } => {
                let (previous_input, previous_output) = self.usage.unwrap_or_default();
                self.usage = Some((
                    previous_input.saturating_add(input_tokens),
                    previous_output.saturating_add(output_tokens),
                ));
            }
            RuntimeUpdate::ContextUsage { used, limit } => {
                self.context_usage = Some((used, limit));
            }
            RuntimeUpdate::Notice(message)
            | RuntimeUpdate::Warning(message)
            | RuntimeUpdate::QuotaPaused(message) => {
                let _ = self.push_runtime_item(ItemKind::Notice, message);
            }
            RuntimeUpdate::Plan(plan) => {
                self.plan = plan
                    .into_iter()
                    .map(|entry| PlanEntry {
                        title: sanitize_text(&entry.title),
                        status: sanitize_text(&entry.status),
                    })
                    .collect();
            }
            RuntimeUpdate::Recovery(state) => {
                if state != RecoveryState::Healthy {
                    let _ =
                        self.push_runtime_item(ItemKind::Notice, format!("recovery: {state:?}"));
                }
            }
            RuntimeUpdate::ToolStuck { name, count } => {
                let _ = self.push_runtime_item(
                    ItemKind::Notice,
                    format!("tool {name} stopped after {count} repeated failures"),
                );
            }
            RuntimeUpdate::Stopped(_) => {
                self.style_active_transcript();
                self.work = WorkState::Idle;
                self.active_assistant = None;
                self.active_reasoning = None;
                self.active_tools.clear();
                self.active_insert_before = None;
            }
        }
        if matches!(self.input_overlay, Some(InputOverlay::TimelineSearch(_))) {
            self.refresh_timeline_search();
        }
    }

    pub fn begin_work(&mut self) {
        self.work = WorkState::Busy {
            cancellation_requested: false,
        };
        self.active_assistant = None;
        self.active_reasoning = None;
        self.active_tools.clear();
        self.stream_bytes = 0;
        self.active_insert_before = None;
    }

    pub fn begin_work_before(&mut self, item: Option<ItemId>) {
        self.begin_work();
        self.active_insert_before = item;
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
        let Some(InputOverlay::TimelineSearch(state)) = &self.input_overlay else {
            return None;
        };
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
        self.input_overlay.is_some()
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
        self.takeover.as_ref().map(|state| TakeoverView {
            kind: state.kind,
            scroll: state.scroll,
            file_scroll: state.file_scroll,
            selected: state.selected,
            commands: &self.command_catalog,
            settings: &state.settings,
            sessions: &state.sessions,
            diff_files: &state.diff_files,
            diff_pane: state.diff_pane,
            selected_file: state.selected_file,
            tree_visible: state.tree_visible,
        })
    }

    #[must_use]
    pub(crate) const fn theme_picker(&self) -> Option<ThemePickerView> {
        match self.theme_picker {
            Some(state) => Some(ThemePickerView {
                original: state.original,
                selected: state.selected,
            }),
            None => None,
        }
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
            sessions: Vec::new(),
            diff_files: Vec::new(),
            diff_pane: DiffPane::Content,
            selected_file: 0,
            tree_visible: true,
        });
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
                })
                .collect(),
            sessions: Vec::new(),
            diff_files: Vec::new(),
            diff_pane: DiffPane::Content,
            selected_file: 0,
            tree_visible: true,
        });
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
            sessions: Vec::new(),
            diff_files,
            diff_pane: DiffPane::Content,
            selected_file: 0,
            tree_visible: true,
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
            sessions,
            diff_files: Vec::new(),
            diff_pane: DiffPane::Content,
            selected_file: 0,
            tree_visible: true,
        });
    }

    pub fn select_takeover_row(&mut self, selected: usize) {
        let Some(state) = self.takeover.as_mut() else {
            return;
        };
        match state.kind {
            TakeoverKind::Settings if selected < state.settings.len() => state.selected = selected,
            TakeoverKind::Sessions if selected < state.sessions.len() => state.selected = selected,
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
            | TakeoverKind::Settings => {}
        }
    }

    pub fn select_diff_file(&mut self, selected: usize) {
        let Some(state) = self.takeover.as_mut() else {
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
        let Some(state) = self.takeover.as_mut() else {
            return;
        };
        if matches!(state.kind, TakeoverKind::Sessions | TakeoverKind::Settings) {
            let total = if state.kind == TakeoverKind::Sessions {
                state.sessions.len()
            } else {
                state.settings.len()
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
        let Some(state) = self.takeover.as_mut() else {
            return;
        };
        if matches!(state.kind, TakeoverKind::Sessions | TakeoverKind::Settings) {
            let total = if state.kind == TakeoverKind::Sessions {
                state.sessions.len()
            } else {
                state.settings.len()
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
        self.input_overlay = None;
    }

    fn cancel_input_overlay(&mut self) {
        if let Some(InputOverlay::TimelineSearch(state)) = self.input_overlay.take() {
            self.editor.restore_snapshot(state.original_draft);
        }
    }

    pub fn seed_history(&mut self, history: Vec<String>) {
        self.editor.seed_history(history);
    }

    /// Clear the current conversation projection while retaining host/session
    /// configuration, completion catalogs and durable prompt history.
    pub fn clear_conversation(&mut self) {
        self.close_theme_picker(true);
        self.timeline = Timeline::new();
        self.work = WorkState::Idle;
        self.stream_bytes = 0;
        self.usage = None;
        self.context_usage = None;
        self.plan.clear();
        self.takeover = None;
        self.quick_help = false;
        self.input_overlay = None;
        self.external_edit_snapshot = None;
        self.active_assistant = None;
        self.active_reasoning = None;
        self.active_tools.clear();
        self.active_insert_before = None;
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

    /// Attach an already-validated image to the ordinary composer. Overlays and
    /// dialogs own input while open, so a host-side Ctrl+V cannot bypass their
    /// containment contract.
    pub fn attach_image(
        &mut self,
        media_type: impl Into<String>,
        data: impl Into<String>,
        byte_len: usize,
    ) -> Option<String> {
        if self.focus != Focus::Composer
            || self.dialog.is_some()
            || self.takeover.is_some()
            || self.theme_picker.is_some()
            || self.input_overlay.is_some()
            || self.shell_mode()
        {
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
        self.timeline.follow_bottom();
        let id = self.timeline.push(ItemKind::User, text)?;
        let _ = self.timeline.set_trailing(id, trailing);
        let _ = self.timeline.set_pending(id, pending);
        if pending
            && matches!(self.work, WorkState::Busy { .. })
            && self.active_insert_before.is_none()
        {
            self.active_insert_before = Some(id);
        }
        Some(id)
    }

    pub fn activate_prompt(&mut self, id: ItemId) -> bool {
        self.timeline.follow_bottom();
        self.timeline.set_pending(id, false)
    }

    /// Insert a stable compact user-shell row. Pending rows intentionally carry
    /// no running activity until their ordered queue position activates.
    pub fn append_shell(&mut self, command: &UserShellCommand, pending: bool) -> Option<ItemId> {
        self.timeline.follow_bottom();
        let id = self
            .timeline
            .push(ItemKind::Shell, format!("Shell {}", command.as_str()))?;
        if pending
            && matches!(self.work, WorkState::Busy { .. })
            && self.active_insert_before.is_none()
        {
            self.active_insert_before = Some(id);
        }
        Some(id)
    }

    pub fn activate_shell(&mut self, id: ItemId) -> bool {
        self.timeline.follow_bottom();
        self.timeline.set_activity(id, Some(ActivityState::Running))
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
        if !self.timeline.replace_text(id, text) {
            return false;
        }
        self.timeline.set_activity(
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
        self.claim_dialog_focus();
        self.dialog = Some(DialogState::Approval {
            tool: sanitize_inline(&tool.into()),
            target: sanitize_inline(&target.into()),
            risk_class: sanitize_inline(&risk_class.into()),
        });
    }

    pub fn request_question(
        &mut self,
        question: impl Into<String>,
        options: impl IntoIterator<Item = String>,
    ) {
        self.claim_dialog_focus();
        let options = options
            .into_iter()
            .map(|option| bounded_inline_text(&option, 1024))
            .filter(|option| !option.is_empty())
            .take(8)
            .collect();
        self.dialog = Some(DialogState::Question(QuestionDialog {
            question: bounded_inline_text(&question.into(), MAX_TOOL_DETAIL_BYTES),
            options,
            selected: 0,
            editing_other: false,
            other: String::new(),
            other_cursor: 0,
        }));
    }

    #[must_use]
    pub(crate) fn question(&self) -> Option<QuestionView<'_>> {
        let Some(DialogState::Question(question)) = &self.dialog else {
            return None;
        };
        Some(QuestionView {
            question: &question.question,
            options: &question.options,
            selected: question.selected,
            editing_other: question.editing_other,
            other: &question.other,
            other_cursor: question.other_cursor,
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
                        let start = previous_char_boundary(
                            &question.other,
                            question.other_cursor.saturating_sub(1),
                        );
                        question.other.drain(start..question.other_cursor);
                        question.other_cursor = start;
                    }
                }
                InputAction::Delete => {
                    if question.other_cursor < question.other.len() {
                        let end = next_char_boundary(
                            &question.other,
                            question.other_cursor.saturating_add(1),
                        );
                        question.other.drain(question.other_cursor..end);
                    }
                }
                InputAction::MoveLeft => {
                    question.other_cursor = previous_char_boundary(
                        &question.other,
                        question.other_cursor.saturating_sub(1),
                    );
                }
                InputAction::MoveRight | InputAction::ForwardCharOrSearch => {
                    question.other_cursor = next_char_boundary(
                        &question.other,
                        question.other_cursor.saturating_add(1),
                    );
                }
                InputAction::Submit => {
                    let answer = question.other.trim();
                    if !answer.is_empty() {
                        return QuestionAction::Submit(answer.to_string());
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
            InputAction::Submit | InputAction::AcceptCompletion => {
                if let Some(answer) = question.options.get(question.selected) {
                    QuestionAction::Submit(answer.clone())
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
    }

    fn claim_dialog_focus(&mut self) {
        self.quick_help = false;
        if self.theme_picker.is_some() {
            self.close_theme_picker(true);
        }
        self.takeover = None;
        if self.input_overlay.is_some() {
            self.cancel_input_overlay();
        }
    }

    pub fn disarm_exit(&mut self) {
        self.exit_armed = false;
    }

    pub fn set_copy_on_select(&mut self, enabled: bool) {
        self.copy_on_select = enabled;
    }

    #[must_use]
    pub const fn copy_on_select(&self) -> bool {
        self.copy_on_select
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

    fn open_timeline_search(&mut self, query: String) {
        self.quick_help = false;
        let original_draft = self.editor.snapshot();
        self.editor.replace_draft(String::new());
        self.input_overlay = Some(InputOverlay::TimelineSearch(TimelineSearchState {
            query: sanitize_text(&query).replace(['\r', '\n'], " "),
            matches: Vec::new(),
            selected: None,
            original_draft,
        }));
        self.refresh_timeline_search();
    }

    fn handle_overlay_input(&mut self, action: InputAction) -> AppCommand {
        if matches!(self.input_overlay, Some(InputOverlay::Completion(_))) {
            return self.handle_completion_input(action);
        }
        if matches!(self.input_overlay, Some(InputOverlay::TimelineSearch(_))) {
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
            | InputAction::AcceptCompletion => {}
        }
        AppCommand::None
    }

    fn handle_takeover_input(&mut self, action: InputAction) -> AppCommand {
        match action {
            InputAction::Escape => {
                self.takeover = None;
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
                AppCommand::None
            }
            InputAction::Submit | InputAction::AcceptCompletion => {
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

    fn handle_theme_picker_input(&mut self, action: InputAction) -> AppCommand {
        match action {
            InputAction::Escape => self.close_theme_picker(true),
            InputAction::MoveUp => self.move_theme_picker(-1),
            InputAction::MoveDown => self.move_theme_picker(1),
            InputAction::Submit | InputAction::AcceptCompletion => self.close_theme_picker(false),
            InputAction::CancelOrExit
            | InputAction::OpenReverseHistory
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
            | InputAction::OpenExternalEditor => {}
        }
        AppCommand::None
    }

    fn move_theme_picker(&mut self, delta: isize) {
        let Some(picker) = self.theme_picker else {
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
            | InputAction::OpenExternalEditor => {}
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
                if let Some(InputOverlay::TimelineSearch(state)) = &mut self.input_overlay {
                    state.query.push_str(&text);
                }
                self.refresh_timeline_search();
            }
            InputAction::Backspace => {
                if let Some(InputOverlay::TimelineSearch(state)) = &mut self.input_overlay {
                    if let Some((byte, _)) = state.query.grapheme_indices(true).next_back() {
                        state.query.truncate(byte);
                    }
                }
                self.refresh_timeline_search();
            }
            InputAction::CancelOrExit
            | InputAction::OpenReverseHistory
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
            | InputAction::AcceptCompletion => {}
        }
        AppCommand::None
    }

    fn refresh_timeline_search(&mut self) {
        let Some(InputOverlay::TimelineSearch(state)) = &self.input_overlay else {
            return;
        };
        let query = state.query.clone();
        let previous = state
            .selected
            .and_then(|selected| state.matches.get(selected))
            .copied();
        let previous_order = previous.and_then(|point| {
            self.timeline
                .items()
                .iter()
                .position(|item| item.id == point.item_id)
        });
        let matches = timeline_search_matches(&self.timeline, &query);
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
                                self.timeline
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
        if let Some(InputOverlay::TimelineSearch(state)) = &mut self.input_overlay {
            state.matches = matches;
            state.selected = selected;
        }
        if let Some(point) = point {
            let _ = self.timeline.hold_at(point);
        }
    }

    fn move_timeline_search(&mut self, delta: isize) {
        let point = {
            let Some(InputOverlay::TimelineSearch(state)) = &mut self.input_overlay else {
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
        let _ = self.timeline.hold_at(point);
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

    fn open_help_for_draft(&mut self) -> bool {
        if self.editor.text().trim() != "/help" {
            return false;
        }
        if self.editor.has_images() {
            let _ = self.push_runtime_item(
                ItemKind::Notice,
                "remove image attachments before opening help",
            );
            return true;
        }
        let _ = self.editor.submit_command();
        self.open_help();
        true
    }

    fn open_theme_for_draft(&mut self) -> bool {
        if self.editor.text().trim() != "/theme" {
            return false;
        }
        if self.editor.has_images() {
            let _ = self.push_runtime_item(
                ItemKind::Notice,
                "remove image attachments before choosing a theme",
            );
            return true;
        }
        let _ = self.editor.submit_command();
        self.open_theme_picker();
        true
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

    fn append_active(&mut self, active: Option<ItemId>, text: &str) -> bool {
        active.is_some_and(|id| self.timeline.append_text(id, text))
    }

    fn push_runtime_item(&mut self, kind: ItemKind, text: impl Into<String>) -> Option<ItemId> {
        let text = text.into();
        if let Some(before) = self.active_insert_before {
            self.timeline.insert_before(before, kind, text)
        } else {
            self.timeline.push(kind, text)
        }
    }

    fn style_active_transcript(&mut self) {
        for id in [self.active_assistant, self.active_reasoning]
            .into_iter()
            .flatten()
        {
            let Some(item) = self.timeline.item(id) else {
                continue;
            };
            let styles = semantic_ranges(item.kind, &item.text);
            let _ = self.timeline.set_styles(id, styles);
        }
    }

    fn style_activity(&mut self, id: ItemId, activity: ActivityState) {
        let Some(end_byte) = self.timeline.item(id).map(|item| item.text.len()) else {
            return;
        };
        let role = match (self.timeline.item(id).map(|item| item.kind), activity) {
            (Some(ItemKind::Question), ActivityState::Running | ActivityState::Success) => {
                SemanticRole::Tool
            }
            (Some(ItemKind::Question), ActivityState::Error | ActivityState::Cancelled) => {
                SemanticRole::Muted
            }
            (_, ActivityState::Running) => SemanticRole::Tool,
            (_, ActivityState::Success) => SemanticRole::Success,
            (_, ActivityState::Error) => SemanticRole::Error,
            (_, ActivityState::Cancelled) => SemanticRole::Muted,
        };
        let styles = (end_byte > 0)
            .then_some(StyledRange {
                start_byte: 0,
                end_byte,
                style: TextStyle::new(role),
            })
            .into_iter()
            .collect();
        let _ = self.timeline.set_styles(id, styles);
    }

    fn cancel_or_exit(&mut self) -> AppCommand {
        if self.exit_armed {
            self.exit_requested = true;
            return AppCommand::Exit;
        }

        self.exit_armed = true;
        if let Some(text) = self.trust_selected_text() {
            return AppCommand::Copy(text);
        }
        if let Some(text) = self.timeline.selected_text() {
            return AppCommand::Copy(text);
        }

        match self.work {
            WorkState::Idle => AppCommand::None,
            WorkState::Busy {
                cancellation_requested: false,
            } => {
                self.work = WorkState::Busy {
                    cancellation_requested: true,
                };
                AppCommand::CancelWork
            }
            WorkState::Busy {
                cancellation_requested: true,
            } => AppCommand::None,
        }
    }

    fn interrupt_work(&mut self) -> AppCommand {
        match self.work {
            WorkState::Busy {
                cancellation_requested: false,
            } => {
                self.work = WorkState::Busy {
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

fn timeline_search_command(text: &str) -> Option<String> {
    let rest = text.strip_prefix("/search")?;
    if rest.is_empty() {
        return Some(String::new());
    }
    rest.chars()
        .next()
        .is_some_and(char::is_whitespace)
        .then(|| rest.trim().to_string())
}

fn timeline_search_matches(timeline: &Timeline, query: &str) -> Vec<ContentPoint> {
    if query.is_empty() {
        return Vec::new();
    }
    timeline
        .items()
        .iter()
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
                mode: "agent".to_string(),
                profile: "default".to_string(),
                session_id: "session".to_string(),
                session_name: None,
            },
            TerminalCapabilities::default(),
        )
    }

    #[test]
    fn usage_updates_accumulate_for_the_whole_session() {
        let mut app = model();
        app.apply_runtime(RuntimeUpdate::Usage {
            input_tokens: 100,
            output_tokens: 20,
        });
        app.apply_runtime(RuntimeUpdate::Usage {
            input_tokens: 7,
            output_tokens: 3,
        });
        assert_eq!(app.usage, Some((107, 23)));
    }

    fn command(name: &str, description: &str) -> CompletionCommand {
        CompletionCommand {
            name: name.to_string(),
            description: description.to_string(),
        }
    }

    #[test]
    fn ctrl_c_arms_then_exits_when_idle_even_with_a_draft() {
        let mut app = model();
        app.editor.insert("unfinished draft");
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::None
        );
        assert!(app.exit_armed);
        assert!(!app.exit_requested);
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::Exit
        );
        assert!(app.exit_requested);
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
            app.work,
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
        let id = app
            .timeline
            .push(ItemKind::Assistant, "copy this")
            .expect("timeline id");
        app.timeline.start_selection(crate::ContentPoint {
            item_id: id,
            byte: 0,
        });
        app.timeline.extend_selection(crate::ContentPoint {
            item_id: id,
            byte: 4,
        });

        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::Copy("copy".to_string())
        );
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
        let id = app.timeline.items()[0].id;
        app.apply_runtime(RuntimeUpdate::Text("world".to_string()));
        assert_eq!(app.timeline.items().len(), 1);
        assert_eq!(app.timeline.items()[0].id, id);
        assert_eq!(app.timeline.items()[0].text, "hello world");
        assert_eq!(app.stream_bytes, "hello world".len());
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
            app.work,
            WorkState::Busy {
                cancellation_requested: true
            }
        );
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
            app.work,
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
        assert!(image.timeline.items().iter().any(|item| {
            item.kind == ItemKind::Notice && item.text.contains("remove image attachments")
        }));
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
            .timeline
            .push(ItemKind::Assistant, "conversation remains underneath")
            .expect("timeline item");
        app.begin_work();
        app.editor.replace_draft("/help");

        assert_eq!(app.handle_input(InputAction::Submit, 80), AppCommand::None);
        assert!(app.has_takeover());
        assert!(app.editor.text().is_empty());
        assert_eq!(
            app.work,
            WorkState::Busy {
                cancellation_requested: false
            }
        );
        assert!(app.timeline.item(existing).is_some());
        assert!(app.attach_image("image/png", "IMAGE_SECRET", 128).is_none());
        assert_eq!(
            app.handle_input(InputAction::MoveDown, 80),
            AppCommand::NavigateTakeover(TakeoverNavigation::LineDown)
        );

        app.apply_runtime(RuntimeUpdate::Text("streamed behind help".to_string()));
        assert_eq!(app.handle_input(InputAction::Escape, 80), AppCommand::None);
        assert!(!app.has_takeover());
        assert_eq!(
            app.work,
            WorkState::Busy {
                cancellation_requested: false
            }
        );
        assert!(app
            .timeline
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
            },
            SettingEntry {
                section: "Session".to_string(),
                name: "Model".to_string(),
                value: "unsafe\u{1b}[2Jmodel\nnext\tvalue".to_string(),
                description: "Current model".to_string(),
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
            .timeline
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
        assert!(app.timeline.item(existing).is_some());
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
            .timeline
            .push(ItemKind::Assistant, "copy me")
            .expect("timeline item");
        app.timeline.start_selection(ContentPoint {
            item_id: item,
            byte: 0,
        });
        app.timeline.extend_selection(ContentPoint {
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
        assert_eq!(app.handle_input(InputAction::Submit, 80), AppCommand::None);
        assert!(app.has_theme_picker());
        assert_eq!(app.theme, Theme::Default);
        assert!(app.attach_image("image/png", "IMAGE_SECRET", 128).is_none());

        let _ = app.handle_input(InputAction::MoveDown, 80);
        assert_eq!(app.theme, Theme::Dim);
        assert_eq!(app.handle_input(InputAction::Escape, 80), AppCommand::None);
        assert!(!app.has_theme_picker());
        assert_eq!(app.theme, Theme::Default);
        assert_eq!(
            app.work,
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
            app.work,
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
        app.timeline.start_selection(ContentPoint {
            item_id: id,
            byte: 0,
        });
        app.timeline.extend_selection(ContentPoint {
            item_id: id,
            byte: submitted.display.len(),
        });
        assert_eq!(
            app.handle_input(InputAction::CancelOrExit, 80),
            AppCommand::Copy(placeholder)
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
            app.work,
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
            app.timeline.item(item).expect("running shell").activity,
            Some(ActivityState::Running)
        );
        let output = UserShellOutput::captured(0, "marker\n", "");
        assert!(app.finish_shell(item, &command, &output));
        let item = app.timeline.item(item).expect("finished shell");
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
        let item = app.timeline.item(item).expect("finished shell");
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
            empty.timeline.item(item).expect("empty result").text,
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
    fn timeline_search_command_counts_messages_and_starts_at_the_newest() {
        let mut app = model();
        let older = app
            .timeline
            .push(ItemKind::User, "marker appears twice: marker")
            .expect("older");
        let newer = app
            .timeline
            .push(ItemKind::Assistant, "newer MARKER")
            .expect("newer");
        let _ = app.handle_input(InputAction::Insert("/search marker".to_string()), 80);

        assert_eq!(app.handle_input(InputAction::Submit, 80), AppCommand::None);
        assert_eq!(app.editor.text(), "");
        assert_eq!(
            app.timeline_search(),
            Some(TimelineSearchView {
                query: "marker",
                current: 2,
                total: 2,
            })
        );
        let ViewportAnchor::Held(point) = app.timeline.viewport else {
            panic!("search should hold the selected match");
        };
        assert_eq!(point.item_id, newer);

        let _ = app.handle_input(InputAction::MoveUp, 80);
        assert_eq!(app.timeline_search().expect("search").current, 1);
        let ViewportAnchor::Held(point) = app.timeline.viewport else {
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
            app.work,
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
        let _ = command.handle_input(InputAction::Insert("/search marker".to_string()), 80);
        let _ = command.handle_input(InputAction::Submit, 80);
        assert_eq!(
            command.handle_input(InputAction::Escape, 80),
            AppCommand::None
        );
        assert_eq!(command.editor.text(), "");
        assert!(!command.exit_armed);
        assert_eq!(
            command.work,
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
            .timeline
            .push(ItemKind::User, "targetx older")
            .expect("older");
        let middle = app
            .timeline
            .push(ItemKind::Assistant, "target middle")
            .expect("middle");
        let newer = app
            .timeline
            .push(ItemKind::Notice, "targetx newer")
            .expect("newer");
        app.open_timeline_search("target".to_string());
        let _ = app.handle_input(InputAction::MoveUp, 80);
        let selected = match app.timeline.viewport {
            ViewportAnchor::Held(point) => point.item_id,
            _ => panic!("held search result"),
        };
        assert_eq!(selected, middle);

        // The selected item no longer matches. Equidistant survivors prefer the
        // newer item so refresh does not jump unpredictably.
        let _ = app.handle_input(InputAction::Insert("x".to_string()), 80);
        let selected = match app.timeline.viewport {
            ViewportAnchor::Held(point) => point.item_id,
            _ => panic!("held fallback result"),
        };
        assert_eq!(selected, newer);
        assert_ne!(selected, older);

        // Appending a new matching item keeps the existing selected identity.
        app.apply_runtime(RuntimeUpdate::Warning("targetx newest".to_string()));
        let selected_after_stream = match app.timeline.viewport {
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
        let viewport = app.timeline.viewport;
        let _ = app.handle_input(InputAction::MoveUp, 80);
        let _ = app.handle_input(InputAction::MoveDown, 80);
        let _ = app.handle_input(InputAction::Submit, 80);
        assert_eq!(app.timeline.viewport, viewport);
        assert!(app.has_input_overlay());
    }

    #[test]
    fn prompt_submission_reanchors_and_pending_state_clears_in_place() {
        let mut app = model();
        let _ = app.timeline.push(ItemKind::Assistant, "old response");
        app.timeline.scroll_by(-1, 20, 1);
        let prompt = app
            .append_prompt("queued", Some("12:34".to_string()), true)
            .expect("prompt");
        assert_eq!(app.timeline.viewport, crate::ViewportAnchor::FollowBottom);
        assert!(app.timeline.item(prompt).expect("prompt").pending);
        assert!(app.activate_prompt(prompt));
        assert!(!app.timeline.item(prompt).expect("prompt").pending);
        assert_eq!(
            app.timeline
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
            .timeline
            .items()
            .iter()
            .map(|item| item.id)
            .collect::<Vec<_>>();
        assert_eq!(ids[0], active);
        assert_eq!(ids[ids.len() - 2..], [queued_a, queued_b]);
        assert_eq!(app.timeline.items()[1].text, "first response");

        app.apply_runtime(RuntimeUpdate::Stopped(StopState::Done));
        assert!(app.activate_prompt(queued_a));
        app.begin_work_before(Some(queued_b));
        app.apply_runtime(RuntimeUpdate::Text("answer a".to_string()));
        let texts = app
            .timeline
            .items()
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            vec![
                "active",
                "first response",
                "inspect",
                "queued a",
                "answer a",
                "queued b"
            ]
        );
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

        let assistant = &app.timeline.items()[0];
        assert!(assistant
            .styles
            .iter()
            .any(|span| span.style.role == SemanticRole::Heading));
        assert!(assistant
            .styles
            .iter()
            .any(|span| span.style.role == SemanticRole::Code));
        let tool = &app.timeline.items()[1];
        assert_eq!(tool.activity, Some(ActivityState::Success));
        assert!(!tool.expanded);
        assert_eq!(app.timeline.rows(80)[2].text, "inspect completed · 1.2 s");
        assert!(tool.text.contains("src/main.rs"));
        assert!(tool.text.contains("detail one\ndetail two"));
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

        let tool = &app.timeline.items()[0];
        assert_eq!(tool.activity, Some(ActivityState::Cancelled));
        assert_eq!(
            app.timeline.rows(80)[0].text,
            "run_shell cancelled · 750 ms"
        );
    }

    #[test]
    fn question_dialog_owns_choices_other_text_and_cancellation() {
        let mut app = model();
        app.request_question("Pick a color", ["Red".to_string(), "Blue".to_string()]);
        assert_eq!(
            app.handle_question_input(InputAction::MoveDown),
            QuestionAction::None
        );
        assert_eq!(
            app.handle_question_input(InputAction::Submit),
            QuestionAction::Submit("Blue".to_string())
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
        assert_eq!(submitted, QuestionAction::Submit("Cya界".to_string()));
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
    fn ask_user_runtime_row_resolves_in_place_with_the_selected_answer() {
        let mut app = model();
        app.apply_runtime(RuntimeUpdate::ToolStarted {
            id: "ask-1".to_string(),
            name: "ask_user".to_string(),
            detail: "Do you prefer Red or Blue?".to_string(),
        });
        assert_eq!(app.timeline.items()[0].kind, ItemKind::Question);
        assert_eq!(
            app.timeline.items()[0].text,
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
        assert_eq!(app.timeline.items().len(), 1);
        assert_eq!(
            app.timeline.items()[0].text,
            "Asked user Do you prefer Red or Blue?\nUser selected: Blue"
        );
        assert_eq!(
            app.timeline.items()[0].activity,
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
    }
}
