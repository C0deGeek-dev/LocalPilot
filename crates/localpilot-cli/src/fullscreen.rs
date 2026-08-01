//! Crossterm host for the backend-neutral full-screen chat model.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::future::Future;
use std::io::{self, Read, Stdout, Write};
use std::panic;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc as std_mpsc, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent,
    MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    self, BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use localpilot_core::ContentBlock;
use localpilot_harness::{ModelHealth, RuntimeEvent, SessionRuntime, StopReason};
use localpilot_terminal_ui::QuestionAction;
use localpilot_terminal_ui::{
    render, AppCommand, AppModel, ColorSupport, CompletionCommand, ContentPoint, DiffFile,
    DiffLine, DiffLineKind, Header, HitMap, InputAction, ItemId, KeyboardSupport, PlanEntry,
    RecoveryState, RuntimeUpdate, SettingEntry, StopState, SubmittedInput, TakeoverNavigation,
    TerminalCapabilities, Theme, TimelineNavigation, UserShellCommand, UserShellOutput,
    VisualRowPart,
};
use localpilot_tools::ElicitationOutcome;
use localpilot_tui::{parse_slash, SlashAction};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::key_input::{is_cancel, is_clipboard_image_key, is_key_action};
use crate::repl::{switch_model_target, ApprovalCall, ClipboardImageRead, ElicitationCall};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const WHEEL_SCROLL_ROWS: isize = 3;
const CHAT_THEME_ENV: &str = "LOCALPILOT_CHAT_THEME";
const CHAT_COPY_ON_SELECT_ENV: &str = "LOCALPILOT_CHAT_COPY_ON_SELECT";
const CHAT_MOUSE_ENV: &str = "LOCALPILOT_CHAT_MOUSE";
const CHAT_SCREEN_READER_ENV: &str = "LOCALPILOT_CHAT_SCREEN_READER";
const CHAT_EDITOR_ENV: &str = "LOCALPILOT_EDITOR";
const MAX_EXTERNAL_EDITOR_BYTES: u64 = 8 * 1024 * 1024;
const MAX_DIFF_BYTES: u64 = 8 * 1024 * 1024;
static TERMINAL_MODES_ACTIVE: AtomicBool = AtomicBool::new(false);
static MOUSE_CAPTURE_ACTIVE: AtomicBool = AtomicBool::new(false);
static KEYBOARD_FLAGS_PUSHED: AtomicBool = AtomicBool::new(false);
static LOCAL_UTC_OFFSET: OnceLock<time::UtcOffset> = OnceLock::new();

pub(crate) fn capture_local_utc_offset() {
    let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let _ = LOCAL_UTC_OFFSET.set(offset);
}

pub(crate) struct HostContext<'a> {
    pub(crate) runtime: &'a mut SessionRuntime,
    pub(crate) approval_rx: &'a mut mpsc::UnboundedReceiver<ApprovalCall>,
    pub(crate) elicitation_rx: &'a mut mpsc::UnboundedReceiver<ElicitationCall>,
    pub(crate) cwd: &'a Path,
    pub(crate) history: &'a localpilot_store::PromptHistory,
    pub(crate) ingest: &'a localpilot_config::IngestConfig,
    pub(crate) config: &'a localpilot_config::Config,
    pub(crate) trust_required: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct QueuedPrompt {
    text: String,
    attachments: Vec<ContentBlock>,
    item_id: ItemId,
}

#[derive(Clone, PartialEq, Eq)]
struct QueuedShell {
    command: UserShellCommand,
    item_id: ItemId,
}

impl std::fmt::Debug for QueuedShell {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QueuedShell")
            .field("command", &self.command)
            .field("item_id", &self.item_id)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
enum QueuedOperation {
    Prompt(QueuedPrompt),
    Shell(QueuedShell),
}

impl QueuedOperation {
    fn item_id(&self) -> ItemId {
        match self {
            Self::Prompt(prompt) => prompt.item_id,
            Self::Shell(shell) => shell.item_id,
        }
    }

    #[cfg(test)]
    fn prompt(&self) -> &QueuedPrompt {
        match self {
            Self::Prompt(prompt) => prompt,
            Self::Shell(_) => panic!("expected queued prompt"),
        }
    }

    #[cfg(test)]
    fn shell(&self) -> &QueuedShell {
        match self {
            Self::Shell(shell) => shell,
            Self::Prompt(_) => panic!("expected queued shell"),
        }
    }
}

impl std::fmt::Debug for QueuedOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Prompt(prompt) => formatter.debug_tuple("Prompt").field(prompt).finish(),
            Self::Shell(shell) => formatter.debug_tuple("Shell").field(shell).finish(),
        }
    }
}

impl std::fmt::Debug for QueuedPrompt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QueuedPrompt")
            .field(
                "text",
                &format_args!("<{} bytes redacted>", self.text.len()),
            )
            .field(
                "attachments",
                &format_args!("<{} redacted>", self.attachments.len()),
            )
            .field("item_id", &self.item_id)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImageCapabilitySnapshot {
    provider_id: String,
    vision_capable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelectionGesture {
    leading: ContentPoint,
    trailing: ContentPoint,
    origin_column: u16,
    origin_row: u16,
}

#[derive(Debug, Default)]
struct MouseState {
    selection: Option<SelectionGesture>,
    selection_pointer: Option<(u16, u16)>,
    scrollbar_grab: Option<u16>,
}

impl MouseState {
    fn reset_gesture(&mut self) {
        self.selection = None;
        self.selection_pointer = None;
        self.scrollbar_grab = None;
    }
}

struct WorkspaceFileIndex {
    receiver: std_mpsc::Receiver<Vec<String>>,
    finished: bool,
}

impl WorkspaceFileIndex {
    fn start(root: PathBuf) -> Self {
        let (sender, receiver) = std_mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("localpilot-workspace-files".to_string())
            .spawn(move || {
                let _ = sender.send(crate::repl::workspace_files(&root));
            });
        Self {
            receiver,
            finished: false,
        }
    }

    fn refresh(&mut self, app: &mut AppModel) {
        if self.finished {
            return;
        }
        match self.receiver.try_recv() {
            Ok(files) => {
                app.set_workspace_files(files);
                self.finished = true;
            }
            Err(std_mpsc::TryRecvError::Disconnected) => {
                app.set_workspace_files(Vec::new());
                self.finished = true;
            }
            Err(std_mpsc::TryRecvError::Empty) => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RoutedEvent {
    Unhandled,
    Handled,
    Copy(String),
}

pub(crate) async fn run(
    header: Header,
    startup_events: impl IntoIterator<Item = RuntimeEvent>,
    context: HostContext<'_>,
) -> Result<()> {
    install_panic_restore_hook();
    let mouse_capture = std::env::var(CHAT_MOUSE_ENV)
        .ok()
        .as_deref()
        .and_then(parse_bool_setting)
        .unwrap_or(true);
    let (mut modes, capabilities) = TerminalModes::enter(mouse_capture)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("initialize full-screen terminal")?;
    terminal.clear().context("clear full-screen terminal")?;
    let mut app = AppModel::new(header, capabilities);
    app.set_command_catalog(fullscreen_command_catalog());
    app.set_command_values(
        "model",
        fullscreen_model_values(context.config, context.runtime.active_provider_id()),
    );
    apply_host_preferences(&mut app);
    for event in startup_events {
        app.apply_runtime(map_runtime_event(event));
    }
    if context.trust_required {
        app.require_workspace_trust(context.cwd.display().to_string());
    }
    // Seat an immediately useful frame before reading even the bounded global
    // history store. Workspace scans stay out of this startup seam entirely.
    let _ = draw_synchronized(&mut terminal, &app)?;
    let mut workspace_index = WorkspaceFileIndex::start(context.cwd.to_path_buf());
    if !context.trust_required {
        crate::repl::start_session_knowledge_index(context.cwd, context.ingest);
    }
    let history_entries = context.history.load();
    app.seed_history(
        localpilot_store::project_entries(&history_entries, context.cwd)
            .iter()
            .map(expand_history_entry)
            .collect(),
    );
    let result = run_event_loop(
        &mut terminal,
        &mut modes,
        &mut app,
        context,
        &mut workspace_index,
    )
    .await;
    let _ = terminal.show_cursor();
    drop(terminal);
    modes.restore();
    result
}

fn fullscreen_command_catalog() -> Vec<CompletionCommand> {
    const SUPPORTED: &[&str] = &["model", "new", "fork", "clone", "clear", "quit"];
    let mut command_catalog = localpilot_tui::AppState::slash_commands()
        .iter()
        .filter(|(name, _)| SUPPORTED.contains(name))
        .map(|(name, description)| CompletionCommand {
            name: (*name).to_string(),
            description: (*description).to_string(),
        })
        .collect::<Vec<_>>();
    command_catalog.push(CompletionCommand {
        name: "search".to_string(),
        description: "Search messages in this session".to_string(),
    });
    command_catalog.push(CompletionCommand {
        name: "help".to_string(),
        description: "Open keyboard and command help".to_string(),
    });
    command_catalog.push(CompletionCommand {
        name: "theme".to_string(),
        description: "Preview terminal color modes".to_string(),
    });
    command_catalog.push(CompletionCommand {
        name: "settings".to_string(),
        description: "Inspect terminal chat settings".to_string(),
    });
    command_catalog.push(CompletionCommand {
        name: "diff".to_string(),
        description: "Review tracked workspace changes".to_string(),
    });
    command_catalog
}

fn load_workspace_diff(cwd: &Path) -> Result<Vec<DiffFile>> {
    let primary = read_git_diff(cwd, true)?;
    let bytes = if primary.0.success() {
        primary.1
    } else {
        let fallback = read_git_diff(cwd, false)?;
        if !fallback.0.success() {
            return Ok(Vec::new());
        }
        fallback.1
    };
    Ok(parse_unified_diff(&String::from_utf8_lossy(&bytes)))
}

fn read_git_diff(cwd: &Path, against_head: bool) -> Result<(std::process::ExitStatus, Vec<u8>)> {
    let mut command = std::process::Command::new("git");
    command.current_dir(cwd).args([
        "-c",
        "core.quotepath=false",
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--unified=3",
    ]);
    if against_head {
        command.arg("HEAD");
    }
    let mut child = command
        .arg("--")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("start git diff")?;
    let mut bytes = Vec::new();
    child
        .stdout
        .take()
        .context("capture git diff output")?
        .take(MAX_DIFF_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read git diff output")?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_DIFF_BYTES {
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!("workspace diff exceeds 8 MiB");
    }
    let status = child.wait().context("wait for git diff")?;
    Ok((status, bytes))
}

fn parse_unified_diff(input: &str) -> Vec<DiffFile> {
    let mut files = Vec::new();
    let mut current: Option<DiffFile> = None;
    let mut old_line = 0u32;
    let mut new_line = 0u32;
    for raw in input.lines() {
        if let Some(header) = raw.strip_prefix("diff --git ") {
            if let Some(file) = current.take() {
                files.push(file);
            }
            let path = split_git_path_fields(header)
                .last()
                .map_or_else(|| header.to_string(), |path| strip_git_prefix(path, "b/"));
            current = Some(DiffFile {
                status: "M".to_string(),
                path,
                additions: 0,
                deletions: 0,
                lines: Vec::new(),
            });
            old_line = 0;
            new_line = 0;
            continue;
        }
        let Some(file) = current.as_mut() else {
            continue;
        };
        if raw.starts_with("new file mode ") {
            file.status = "A".to_string();
            continue;
        }
        if raw.starts_with("deleted file mode ") {
            file.status = "D".to_string();
            continue;
        }
        if let Some(path) = raw.strip_prefix("rename to ") {
            file.status = "R".to_string();
            file.path = decode_git_path(path);
            continue;
        }
        if let Some(path) = raw.strip_prefix("copy to ") {
            file.status = "C".to_string();
            file.path = decode_git_path(path);
            continue;
        }
        if let Some(path) = raw.strip_prefix("+++ ") {
            if path != "/dev/null" {
                file.path = strip_git_prefix(&decode_git_path(path), "b/");
            }
            continue;
        }
        if raw.starts_with("--- ") || raw.starts_with("index ") {
            continue;
        }
        if raw.starts_with("@@") {
            if let Some((old, new)) = parse_hunk_starts(raw) {
                old_line = old;
                new_line = new;
            }
            file.lines.push(DiffLine {
                old_line: None,
                new_line: None,
                kind: DiffLineKind::Hunk,
                text: raw.to_string(),
            });
            continue;
        }
        let (kind, old, new, text) = if let Some(text) = raw.strip_prefix('+') {
            let line = new_line;
            new_line = new_line.saturating_add(1);
            file.additions = file.additions.saturating_add(1);
            (DiffLineKind::Addition, None, Some(line), text)
        } else if let Some(text) = raw.strip_prefix('-') {
            let line = old_line;
            old_line = old_line.saturating_add(1);
            file.deletions = file.deletions.saturating_add(1);
            (DiffLineKind::Deletion, Some(line), None, text)
        } else if let Some(text) = raw.strip_prefix(' ') {
            let old = old_line;
            let new = new_line;
            old_line = old_line.saturating_add(1);
            new_line = new_line.saturating_add(1);
            (DiffLineKind::Context, Some(old), Some(new), text)
        } else {
            (DiffLineKind::Metadata, None, None, raw)
        };
        file.lines.push(DiffLine {
            old_line: old,
            new_line: new,
            kind,
            text: text.to_string(),
        });
    }
    if let Some(file) = current {
        files.push(file);
    }
    files
}

fn split_git_path_fields(input: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for character in input.chars() {
        if quoted {
            if escaped {
                current.push('\\');
                current.push(character);
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quoted = false;
            } else {
                current.push(character);
            }
        } else if character == '"' && current.is_empty() {
            quoted = true;
        } else if character.is_whitespace() {
            if !current.is_empty() {
                fields.push(decode_git_quoted(&current));
                current.clear();
            }
        } else {
            current.push(character);
        }
    }
    if escaped {
        current.push('\\');
    }
    if !current.is_empty() {
        fields.push(decode_git_quoted(&current));
    }
    fields
}

fn decode_git_path(input: &str) -> String {
    let trimmed = input.trim();
    if let Some(body) = trimmed
        .strip_prefix('"')
        .and_then(|body| body.strip_suffix('"'))
    {
        decode_git_quoted(body)
    } else {
        trimmed.to_string()
    }
}

fn decode_git_quoted(input: &str) -> String {
    let input = input.as_bytes();
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] != b'\\' || index + 1 >= input.len() {
            output.push(input[index]);
            index += 1;
            continue;
        }
        index += 1;
        match input[index] {
            b'a' => output.push(0x07),
            b'b' => output.push(0x08),
            b't' => output.push(b'\t'),
            b'n' => output.push(b'\n'),
            b'v' => output.push(0x0b),
            b'f' => output.push(0x0c),
            b'r' => output.push(b'\r'),
            digit @ b'0'..=b'7' => {
                let mut value = digit - b'0';
                let mut digits = 1;
                while digits < 3
                    && index + 1 < input.len()
                    && matches!(input[index + 1], b'0'..=b'7')
                {
                    index += 1;
                    value = value.saturating_mul(8).saturating_add(input[index] - b'0');
                    digits += 1;
                }
                output.push(value);
            }
            escaped => output.push(escaped),
        }
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn strip_git_prefix(path: &str, prefix: &str) -> String {
    path.strip_prefix(prefix).unwrap_or(path).to_string()
}

fn parse_hunk_starts(header: &str) -> Option<(u32, u32)> {
    let mut fields = header.split_whitespace();
    (fields.next()? == "@@").then_some(())?;
    let old = fields
        .next()?
        .strip_prefix('-')?
        .split(',')
        .next()?
        .parse()
        .ok()?;
    let new = fields
        .next()?
        .strip_prefix('+')?
        .split(',')
        .next()?
        .parse()
        .ok()?;
    Some((old, new))
}

fn fullscreen_settings(app: &AppModel) -> Vec<SettingEntry> {
    let enabled = |value| if value { "On" } else { "Off" }.to_string();
    vec![
        SettingEntry {
            section: "Input".to_string(),
            name: "Mouse reporting".to_string(),
            value: enabled(app.capabilities.mouse_capture),
            description: format!(
                "Set {CHAT_MOUSE_ENV}=false before launch for keyboard-only input."
            ),
        },
        SettingEntry {
            section: "Input".to_string(),
            name: "Copy on selection".to_string(),
            value: enabled(app.copy_on_select()),
            description: format!(
                "Set {CHAT_COPY_ON_SELECT_ENV}=true to copy immediately after a drag selection."
            ),
        },
        SettingEntry {
            section: "Input".to_string(),
            name: "Clipboard".to_string(),
            value: if app.capabilities.clipboard_write {
                "Available"
            } else {
                "Unavailable"
            }
            .to_string(),
            description: "Ctrl+C and right-click copy use the platform clipboard when available."
                .to_string(),
        },
        SettingEntry {
            section: "Input".to_string(),
            name: "Keyboard protocol".to_string(),
            value: match app.capabilities.keyboard {
                KeyboardSupport::Basic => "Basic",
                KeyboardSupport::Enhanced => "Enhanced",
            }
            .to_string(),
            description: "Enhanced reporting distinguishes more modified key combinations."
                .to_string(),
        },
        SettingEntry {
            section: "Accessibility".to_string(),
            name: "Screen reader".to_string(),
            value: enabled(app.capabilities.screen_reader),
            description: format!(
                "Set {CHAT_SCREEN_READER_ENV}=true for a role-labeled full-screen projection."
            ),
        },
        SettingEntry {
            section: "Appearance".to_string(),
            name: "Color mode".to_string(),
            value: app.theme.display_name().to_string(),
            description: format!(
                "Use /theme to preview modes or set {CHAT_THEME_ENV} before launch."
            ),
        },
        SettingEntry {
            section: "Session".to_string(),
            name: "Provider".to_string(),
            value: app.header.provider.clone(),
            description: "The provider currently serving this conversation.".to_string(),
        },
        SettingEntry {
            section: "Session".to_string(),
            name: "Model".to_string(),
            value: app.header.model.clone(),
            description: "Use /model to choose from configured LocalPilot providers.".to_string(),
        },
        SettingEntry {
            section: "Session".to_string(),
            name: "Mode and profile".to_string(),
            value: format!("{} · {}", app.header.mode, app.header.profile),
            description: "The active LocalPilot execution mode and permission profile.".to_string(),
        },
    ]
}

fn fullscreen_model_values(
    config: &localpilot_config::Config,
    active_provider: &str,
) -> Vec<CompletionCommand> {
    config
        .providers
        .iter()
        .map(|(id, provider)| {
            let active = if id == active_provider {
                "current · "
            } else {
                ""
            };
            let model = provider.model.as_deref().unwrap_or("provider default");
            CompletionCommand {
                name: id.clone(),
                description: format!("{active}{} · {model}", provider.kind),
            }
        })
        .collect()
}

fn image_content_blocks(images: Vec<localpilot_terminal_ui::ImageAttachment>) -> Vec<ContentBlock> {
    images
        .into_iter()
        .map(|image| ContentBlock::image(image.media_type, image.data))
        .collect()
}

fn apply_host_preferences(app: &mut AppModel) {
    if let Some(value) = std::env::var_os(CHAT_THEME_ENV) {
        match value.into_string() {
            Ok(value) => match value.parse::<Theme>() {
                Ok(theme) => app.theme = theme,
                Err(error) => app.apply_runtime(RuntimeUpdate::Warning(error.to_string())),
            },
            Err(_) => app.apply_runtime(RuntimeUpdate::Warning(format!(
                "{CHAT_THEME_ENV} contains non-Unicode text; using the default theme"
            ))),
        }
    }
    if let Some(value) = std::env::var_os(CHAT_COPY_ON_SELECT_ENV) {
        match value.into_string() {
            Ok(value) => match parse_bool_setting(&value) {
                Some(enabled) => app.set_copy_on_select(enabled),
                None => app.apply_runtime(RuntimeUpdate::Warning(format!(
                    "{CHAT_COPY_ON_SELECT_ENV} must be true, false, 1, or 0; using false"
                ))),
            },
            Err(_) => app.apply_runtime(RuntimeUpdate::Warning(format!(
                "{CHAT_COPY_ON_SELECT_ENV} must be true, false, 1, or 0; using false"
            ))),
        }
    }
    if std::env::var_os(CHAT_MOUSE_ENV).is_some()
        && std::env::var(CHAT_MOUSE_ENV)
            .ok()
            .as_deref()
            .and_then(parse_bool_setting)
            .is_none()
    {
        app.apply_runtime(RuntimeUpdate::Warning(format!(
            "{CHAT_MOUSE_ENV} must be true, false, 1, or 0; using true"
        )));
    }
    if std::env::var_os(CHAT_SCREEN_READER_ENV).is_some()
        && std::env::var(CHAT_SCREEN_READER_ENV)
            .ok()
            .as_deref()
            .and_then(parse_bool_setting)
            .is_none()
    {
        app.apply_runtime(RuntimeUpdate::Warning(format!(
            "{CHAT_SCREEN_READER_ENV} must be true, false, 1, or 0; using false"
        )));
    }
}

fn parse_bool_setting(value: &str) -> Option<bool> {
    if value.eq_ignore_ascii_case("true") || value == "1" {
        Some(true)
    } else if value.eq_ignore_ascii_case("false") || value == "0" {
        Some(false)
    } else {
        None
    }
}

async fn attach_clipboard_image_idle(
    app: &mut AppModel,
    runtime: &mut SessionRuntime,
    config: &localpilot_config::Config,
    quiet_when_absent: bool,
) {
    if app.has_input_overlay() || app.has_takeover() || app.has_theme_picker() {
        return;
    }
    if app.shell_mode() {
        app.apply_runtime(RuntimeUpdate::Warning(
            "images are not available in shell mode".to_string(),
        ));
        return;
    }
    let provider_id = runtime.active_provider_id().to_string();
    if !runtime.active_accepts_images() {
        let resolved = crate::repl::resolved_image_support(config, Some(&provider_id)).await;
        runtime.set_image_support_override(resolved);
    }
    let capability = ImageCapabilitySnapshot {
        provider_id,
        vision_capable: runtime.active_accepts_images(),
    };
    attach_clipboard_image_with_capability(app, &capability, quiet_when_absent);
}

fn attach_clipboard_image_with_capability(
    app: &mut AppModel,
    capability: &ImageCapabilitySnapshot,
    quiet_when_absent: bool,
) {
    if app.has_input_overlay() || app.has_takeover() || app.has_theme_picker() {
        return;
    }
    if app.shell_mode() {
        app.apply_runtime(RuntimeUpdate::Warning(
            "images are not available in shell mode".to_string(),
        ));
        return;
    }
    if !capability.vision_capable {
        app.apply_runtime(RuntimeUpdate::Warning(
            crate::repl::image_unsupported_notice(&capability.provider_id),
        ));
        return;
    }
    let image = match crate::repl::read_clipboard_image() {
        Ok(ClipboardImageRead::Missing) => {
            if !quiet_when_absent {
                app.apply_runtime(RuntimeUpdate::Warning(
                    "no image on the clipboard".to_string(),
                ));
            }
            return;
        }
        Ok(ClipboardImageRead::Image(image)) => image,
        Err(message) => {
            app.apply_runtime(RuntimeUpdate::Warning(message));
            return;
        }
    };
    let crate::repl::CapturedClipboardImage {
        media_type,
        data,
        byte_len,
        width,
        height,
    } = image;
    if app.attach_image(media_type, data, byte_len).is_some() {
        app.apply_runtime(RuntimeUpdate::Warning(format!(
            "attached {width}×{height} image"
        )));
    }
}

#[derive(Clone, PartialEq, Eq)]
struct EditorCommand {
    program: OsString,
    args: Vec<OsString>,
}

impl std::fmt::Debug for EditorCommand {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EditorCommand")
            .field("program", &self.program)
            .field("args", &format_args!("<{} redacted>", self.args.len()))
            .finish()
    }
}

fn resolve_editor_command() -> Result<EditorCommand> {
    resolve_editor_command_with(|name| std::env::var_os(name))
        .map_err(anyhow::Error::msg)
        .context("resolve external editor")
}

fn resolve_editor_command_with(
    mut lookup: impl FnMut(&str) -> Option<OsString>,
) -> std::result::Result<EditorCommand, String> {
    for name in [CHAT_EDITOR_ENV, "VISUAL", "EDITOR"] {
        let Some(value) = lookup(name) else {
            continue;
        };
        let value = value
            .into_string()
            .map_err(|_| format!("{name} contains non-Unicode text"))?;
        let mut parts = split_editor_command(&value)?;
        let Some(program) = parts.first().cloned() else {
            return Err(format!("{name} is empty"));
        };
        parts.remove(0);
        return Ok(EditorCommand {
            program,
            args: parts,
        });
    }
    Ok(EditorCommand {
        program: if cfg!(windows) {
            OsString::from("notepad.exe")
        } else {
            OsString::from("vi")
        },
        args: Vec::new(),
    })
}

fn split_editor_command(value: &str) -> std::result::Result<Vec<OsString>, String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut token_started = false;
    for character in value.trim().chars() {
        match (quote, character) {
            (Some(open), current_quote) if open == current_quote => {
                quote = None;
                token_started = true;
            }
            (None, '"' | '\'') => {
                quote = Some(character);
                token_started = true;
            }
            (None, whitespace) if whitespace.is_whitespace() => {
                if token_started {
                    parts.push(OsString::from(std::mem::take(&mut current)));
                    token_started = false;
                }
            }
            _ => {
                current.push(character);
                token_started = true;
            }
        }
    }
    if quote.is_some() {
        return Err("external editor command has an unterminated quote".to_string());
    }
    if token_started {
        parts.push(OsString::from(current));
    }
    Ok(parts)
}

trait SuspensibleModes {
    type Capabilities;

    fn leave(&mut self);
    fn reenter(&mut self) -> Result<Self::Capabilities>;
}

struct ModeSuspension<'a, M: SuspensibleModes> {
    modes: &'a mut M,
    reentry_attempted: bool,
}

impl<'a, M: SuspensibleModes> ModeSuspension<'a, M> {
    fn new(modes: &'a mut M) -> Self {
        modes.leave();
        Self {
            modes,
            reentry_attempted: false,
        }
    }

    fn resume(mut self) -> Result<M::Capabilities> {
        self.reentry_attempted = true;
        self.modes.reenter()
    }
}

impl<M: SuspensibleModes> Drop for ModeSuspension<'_, M> {
    fn drop(&mut self) {
        // An early non-panic return still restores the application. During a
        // panic, remaining in the already-restored plain terminal is safer; the
        // panic hook cannot run a second time after unwinding re-enters modes.
        if !self.reentry_attempted && !std::thread::panicking() {
            let _ = self.modes.reenter();
        }
    }
}

async fn with_modes_suspended<M, F, T>(modes: &mut M, operation: F) -> Result<(T, M::Capabilities)>
where
    M: SuspensibleModes,
    F: Future<Output = T>,
{
    let suspension = ModeSuspension::new(modes);
    let output = operation.await;
    let capabilities = suspension.resume()?;
    Ok((output, capabilities))
}

async fn launch_external_editor(command: &EditorCommand, path: &Path) -> Result<()> {
    let status = tokio::process::Command::new(&command.program)
        .args(&command.args)
        .arg(path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("start editor {}", command.program.to_string_lossy()))?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("external editor exited with {status}"))
    }
}

fn read_external_edit(path: &Path) -> Result<String> {
    let size = std::fs::metadata(path)
        .context("inspect edited prompt")?
        .len();
    if size > MAX_EXTERNAL_EDITOR_BYTES {
        anyhow::bail!("edited prompt exceeds the 8 MiB limit");
    }
    let bytes = std::fs::read(path).context("read edited prompt")?;
    String::from_utf8(bytes).context("edited prompt is not valid UTF-8")
}

async fn edit_composer_externally(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    modes: &mut TerminalModes,
    app: &mut AppModel,
) -> Result<()> {
    let Some(draft) = app.external_edit_text().map(str::to_owned) else {
        return Ok(());
    };
    let prepared = (|| -> Result<_> {
        let directory = tempfile::Builder::new()
            .prefix("localpilot-edit-")
            .tempdir()
            .context("create external-editor directory")?;
        let path = directory.path().join("LOCALPILOT_PROMPT.md");
        std::fs::write(&path, draft).context("write external-editor draft")?;
        let command = resolve_editor_command()?;
        Ok((directory, path, command))
    })();
    let (directory, path, command) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            app.finish_external_edit(None);
            app.apply_runtime(RuntimeUpdate::Warning(format!(
                "external editor could not start: {error}"
            )));
            return Ok(());
        }
    };

    let _ = terminal.show_cursor();
    let operation = async {
        let mut stdout = io::stdout();
        writeln!(
            stdout,
            "Editing prompt; close the editor to return to LocalPilot…"
        )
        .context("write external-editor handoff")?;
        stdout.flush().context("flush external-editor handoff")?;
        launch_external_editor(&command, &path).await?;
        read_external_edit(&path)
    };
    let (edited, capabilities) = with_modes_suspended(modes, operation).await?;
    app.capabilities = capabilities;
    drop(directory);
    match edited {
        Ok(edited) => app.finish_external_edit(Some(edited)),
        Err(error) => {
            app.finish_external_edit(None);
            app.apply_runtime(RuntimeUpdate::Warning(format!(
                "external editor kept the original draft: {error}"
            )));
        }
    }
    terminal.clear().context("clear after external editor")?;
    let _ = draw_synchronized(terminal, app)?;
    Ok(())
}

fn prepare_prompt_operation(
    app: &mut AppModel,
    history: &localpilot_store::PromptHistory,
    cwd: &Path,
    submitted: SubmittedInput,
    pending: bool,
) -> Option<QueuedOperation> {
    let item_id = app.append_prompt(
        submitted.display.clone(),
        Some(local_prompt_time()),
        pending,
    )?;
    persist_prompt(app, history, cwd, &submitted);
    let attachments = image_content_blocks(submitted.images);
    if !attachments.is_empty() {
        app.apply_runtime(RuntimeUpdate::Warning(format!(
            "sending {} image(s) with this prompt",
            attachments.len()
        )));
    }
    Some(QueuedOperation::Prompt(QueuedPrompt {
        text: submitted.prompt,
        attachments,
        item_id,
    }))
}

fn prepare_shell_operation(
    app: &mut AppModel,
    command: UserShellCommand,
    pending: bool,
) -> Option<QueuedOperation> {
    let item_id = app.append_shell(&command, pending)?;
    Some(QueuedOperation::Shell(QueuedShell { command, item_id }))
}

async fn execute_fullscreen_slash(
    app: &mut AppModel,
    runtime: &mut SessionRuntime,
    config: &localpilot_config::Config,
    cwd: &Path,
    submitted: SubmittedInput,
) -> bool {
    if !submitted.images.is_empty() {
        app.apply_runtime(RuntimeUpdate::Notice(
            "image attachments were ignored for the slash command".to_string(),
        ));
    }
    if submitted.prompt.trim() == "/settings" {
        let settings = fullscreen_settings(app);
        app.open_settings(settings);
        return false;
    }
    if submitted.prompt.trim() == "/diff" {
        match load_workspace_diff(cwd) {
            Ok(files) => app.open_diff(files),
            Err(error) => {
                app.open_diff([DiffFile {
                    status: "!".to_string(),
                    path: "Diff unavailable".to_string(),
                    additions: 0,
                    deletions: 0,
                    lines: vec![DiffLine {
                        old_line: None,
                        new_line: None,
                        kind: DiffLineKind::Metadata,
                        text: error.to_string(),
                    }],
                }]);
            }
        }
        return false;
    }
    let Some(action) = parse_slash(&submitted.prompt) else {
        app.apply_runtime(RuntimeUpdate::Warning(
            "invalid slash command input".to_string(),
        ));
        return false;
    };
    match action {
        SlashAction::Model {
            provider: Some(provider),
            model,
        } => {
            let report = switch_model_target(runtime, config, &provider, model).await;
            app.header.provider = report.provider;
            app.header.model = report.model;
            for notice in report.notices {
                app.apply_runtime(RuntimeUpdate::Notice(notice));
            }
        }
        SlashAction::Model { provider: None, .. } => {
            let values = fullscreen_model_values(config, runtime.active_provider_id());
            if values.is_empty() {
                app.apply_runtime(RuntimeUpdate::Notice(
                    "no providers are configured".to_string(),
                ));
            } else {
                app.apply_runtime(RuntimeUpdate::Notice(
                    "type /model <provider> or choose one from the completion list".to_string(),
                ));
            }
        }
        SlashAction::Clear => {
            runtime.clear_conversation();
            app.clear_conversation();
            let (used, limit) = runtime.context_usage();
            app.apply_runtime(RuntimeUpdate::ContextUsage { used, limit });
            app.apply_runtime(RuntimeUpdate::Notice("conversation cleared".to_string()));
        }
        SlashAction::NewSession => {
            runtime.start_new_session();
            app.clear_conversation();
            app.header.session_id = runtime.session_id().to_string();
            app.header.session_name = None;
            let (used, limit) = runtime.context_usage();
            app.apply_runtime(RuntimeUpdate::ContextUsage { used, limit });
            app.apply_runtime(RuntimeUpdate::Notice(format!(
                "started new session {}",
                runtime.session_id()
            )));
        }
        action @ (SlashAction::Fork | SlashAction::CloneSession) => {
            let mark_fork = matches!(action, SlashAction::Fork);
            match runtime.fork_session(mark_fork) {
                Ok(id) => {
                    app.header.session_id = id.to_string();
                    app.header.session_name = None;
                    let verb = if mark_fork { "forked" } else { "cloned" };
                    app.apply_runtime(RuntimeUpdate::Notice(format!("{verb} into session {id}")));
                }
                Err(error) => app.apply_runtime(RuntimeUpdate::Warning(format!(
                    "session branch failed: {error}"
                ))),
            }
        }
        SlashAction::Quit => return true,
        SlashAction::Invalid { command, reason } => {
            app.apply_runtime(RuntimeUpdate::Warning(format!(
                "invalid /{command}: {reason}"
            )));
        }
        SlashAction::Unknown(command) => {
            app.apply_runtime(RuntimeUpdate::Warning(format!(
                "unknown slash command: /{command}"
            )));
        }
        _ => {
            let command = submitted
                .prompt
                .trim()
                .trim_start_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or("command");
            app.apply_runtime(RuntimeUpdate::Notice(format!(
                "/{command} is not available in full-screen chat yet"
            )));
        }
    }
    false
}

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    modes: &mut TerminalModes,
    app: &mut AppModel,
    context: HostContext<'_>,
    workspace_index: &mut WorkspaceFileIndex,
) -> Result<()> {
    let HostContext {
        runtime,
        approval_rx,
        elicitation_rx,
        cwd,
        history,
        ingest,
        config,
        trust_required: _,
    } = context;
    let mut queue = VecDeque::new();
    let mut mouse_state = MouseState::default();
    while !app.exit_requested {
        workspace_index.refresh(app);
        let hit_map = draw_synchronized(terminal, app)?;
        if !event::poll(EVENT_POLL_INTERVAL).context("poll full-screen terminal event")? {
            advance_mouse_selection(app, &hit_map, &mouse_state);
            continue;
        }
        let next = event::read().context("read full-screen terminal event")?;
        if app.workspace_trust_pending() {
            mouse_state.reset_gesture();
            if handle_trust_event(app, next, cwd, ingest) {
                break;
            }
            continue;
        }
        match route_pointer_or_navigation(app, &next, &hit_map, &mut mouse_state) {
            RoutedEvent::Handled => continue,
            RoutedEvent::Copy(text) => {
                copy_to_clipboard(app, text);
                continue;
            }
            RoutedEvent::Unhandled => {}
        }
        match next {
            Event::Key(key) if is_key_action(key) => {
                if is_clipboard_image_key(key) {
                    attach_clipboard_image_idle(app, runtime, config, false).await;
                    continue;
                }
                let Some(action) = map_key(key) else {
                    continue;
                };
                match app.handle_input(action, hit_map.editor_width) {
                    AppCommand::Exit => break,
                    AppCommand::Copy(text) => copy_to_clipboard(app, text),
                    AppCommand::OpenExternalEditor => {
                        edit_composer_externally(terminal, modes, app).await?;
                    }
                    AppCommand::RunSlash(submitted) => {
                        if execute_fullscreen_slash(app, runtime, config, cwd, submitted).await {
                            break;
                        }
                    }
                    AppCommand::Submit(submitted) => {
                        let Some(operation) =
                            prepare_prompt_operation(app, history, cwd, submitted, false)
                        else {
                            continue;
                        };
                        if drive_operation_chain(
                            terminal,
                            app,
                            runtime,
                            approval_rx,
                            elicitation_rx,
                            operation,
                            &mut queue,
                            history,
                            cwd,
                            &mut mouse_state,
                            workspace_index,
                        )
                        .await?
                        {
                            break;
                        }
                    }
                    AppCommand::RunShell(command) => {
                        let Some(operation) = prepare_shell_operation(app, command, false) else {
                            continue;
                        };
                        if drive_operation_chain(
                            terminal,
                            app,
                            runtime,
                            approval_rx,
                            elicitation_rx,
                            operation,
                            &mut queue,
                            history,
                            cwd,
                            &mut mouse_state,
                            workspace_index,
                        )
                        .await?
                        {
                            break;
                        }
                    }
                    AppCommand::NavigateTakeover(navigation) => {
                        apply_takeover_navigation(app, navigation, &hit_map);
                    }
                    AppCommand::NavigateTimeline(navigation) => {
                        apply_timeline_navigation(app, navigation, &hit_map);
                    }
                    AppCommand::None | AppCommand::CancelWork => {}
                }
            }
            Event::Paste(text) => {
                if text.trim().is_empty() {
                    attach_clipboard_image_idle(app, runtime, config, true).await;
                } else {
                    let _ = app.handle_input(InputAction::Paste(text), hit_map.editor_width);
                }
            }
            Event::FocusGained
            | Event::FocusLost
            | Event::Resize(_, _)
            | Event::Mouse(_)
            | Event::Key(_) => {}
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn drive_operation_chain(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppModel,
    runtime: &mut SessionRuntime,
    approval_rx: &mut mpsc::UnboundedReceiver<ApprovalCall>,
    elicitation_rx: &mut mpsc::UnboundedReceiver<ElicitationCall>,
    first: QueuedOperation,
    queue: &mut VecDeque<QueuedOperation>,
    history: &localpilot_store::PromptHistory,
    cwd: &Path,
    mouse_state: &mut MouseState,
    workspace_index: &mut WorkspaceFileIndex,
) -> Result<bool> {
    let mut current = Some(first);
    while let Some(operation) = current {
        let next_item = queue.front().map(QueuedOperation::item_id);
        match operation {
            QueuedOperation::Prompt(prompt) => {
                let _ = app.activate_prompt(prompt.item_id);
                app.begin_work_before(next_item);
                if drive_turn(
                    terminal,
                    app,
                    runtime,
                    approval_rx,
                    elicitation_rx,
                    &prompt.text,
                    &prompt.attachments,
                    queue,
                    history,
                    cwd,
                    mouse_state,
                    workspace_index,
                )
                .await?
                {
                    return Ok(true);
                }
            }
            QueuedOperation::Shell(shell) => {
                app.begin_work_before(next_item);
                let _ = app.activate_shell(shell.item_id);
                if drive_shell(
                    terminal,
                    app,
                    runtime,
                    approval_rx,
                    shell,
                    queue,
                    history,
                    cwd,
                    mouse_state,
                    workspace_index,
                )
                .await?
                {
                    return Ok(true);
                }
            }
        }
        current = queue.pop_front();
    }
    Ok(false)
}

#[allow(clippy::too_many_arguments)] // the live terminal pump threads these owners
async fn drive_shell(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppModel,
    runtime: &mut SessionRuntime,
    approval_rx: &mut mpsc::UnboundedReceiver<ApprovalCall>,
    shell: QueuedShell,
    queue: &mut VecDeque<QueuedOperation>,
    history: &localpilot_store::PromptHistory,
    cwd: &Path,
    mouse_state: &mut MouseState,
    workspace_index: &mut WorkspaceFileIndex,
) -> Result<bool> {
    let cancel = CancellationToken::new();
    let image_capability = ImageCapabilitySnapshot {
        provider_id: runtime.active_provider_id().to_string(),
        vision_capable: runtime.active_accepts_images(),
    };
    let mut pending: Option<oneshot::Sender<bool>> = None;
    let outcome = {
        let operation =
            runtime.run_user_shell_command_detailed(shell.command.as_str(), &cancel, false);
        tokio::pin!(operation);
        let mut tick = tokio::time::interval(EVENT_POLL_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        async {
            loop {
        tokio::select! {
            biased;
            _ = tick.tick() => {
                workspace_index.refresh(app);
                let mut hit_map = draw_synchronized(terminal, app)?;
                for _ in 0..64 {
                    if !event::poll(Duration::ZERO).context("poll full-screen shell input")? {
                        break;
                    }
                    let next = event::read().context("read full-screen shell input")?;
                    let geometry_event = matches!(next, Event::Mouse(_) | Event::Resize(_, _));
                    let exit = if pending.is_some() {
                        handle_approval_event(app, next, &mut pending, &cancel)
                    } else {
                        match route_pointer_or_navigation(app, &next, &hit_map, mouse_state) {
                            RoutedEvent::Handled => false,
                            RoutedEvent::Copy(text) => {
                                copy_to_clipboard(app, text);
                                false
                            }
                            RoutedEvent::Unhandled => handle_turn_event(
                                app,
                                next,
                                &cancel,
                                &hit_map,
                                queue,
                                history,
                                cwd,
                                &image_capability,
                            ),
                        }
                    };
                    if exit {
                        cancel.cancel();
                        return Ok(true);
                    }
                    if geometry_event {
                        hit_map = draw_synchronized(terminal, app)?;
                    }
                }
                advance_mouse_selection(app, &hit_map, mouse_state);
                let _ = draw_synchronized(terminal, app)?;
            }
            result = &mut operation => {
                let output = result.shell.map_or_else(
                    || {
                        UserShellOutput::diagnostic(
                            result.result.is_error,
                            present_shell_diagnostic(&result.result.output),
                        )
                    },
                    |captured| {
                        UserShellOutput::captured(
                            captured.exit_code,
                            &captured.stdout,
                            &captured.stderr,
                        )
                    },
                );
                let _ = app.finish_shell(shell.item_id, &shell.command, &output);
                app.apply_runtime(RuntimeUpdate::Stopped(StopState::Done));
                let _ = draw_synchronized(terminal, app)?;
                return Ok(false);
            }
            Some(call) = approval_rx.recv(), if pending.is_none() => {
                mouse_state.reset_gesture();
                app.request_approval(
                    call.request.tool,
                    call.request.target,
                    call.request.risk_class,
                );
                pending = Some(call.reply);
            }
        }
            }
        }
        .await
    };
    deny_pending(app, &mut pending);
    deny_buffered_approvals(approval_rx);
    outcome
}

fn handle_trust_event(
    app: &mut AppModel,
    event: Event,
    cwd: &Path,
    ingest: &localpilot_config::IngestConfig,
) -> bool {
    let Event::Key(key) = event else {
        if matches!(event, Event::Paste(_)) {
            app.disarm_exit();
        }
        return false;
    };
    if !is_key_action(key) {
        return false;
    }
    if !is_cancel(key) {
        app.disarm_exit();
    }
    match key.code {
        KeyCode::Char('y' | 'Y') => {
            crate::trust::remember(cwd);
            crate::repl::start_session_knowledge_index(cwd, ingest);
            app.clear_dialog();
            false
        }
        KeyCode::Enter if !app.capabilities.screen_reader => {
            crate::trust::remember(cwd);
            crate::repl::start_session_knowledge_index(cwd, ingest);
            app.clear_dialog();
            false
        }
        KeyCode::Char('n' | 'N') | KeyCode::Esc => true,
        _ if is_cancel(key) => matches!(
            app.handle_input(InputAction::CancelOrExit, 1),
            AppCommand::Exit
        ),
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)] // the live terminal pump threads these owners
async fn drive_turn(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppModel,
    runtime: &mut SessionRuntime,
    approval_rx: &mut mpsc::UnboundedReceiver<ApprovalCall>,
    elicitation_rx: &mut mpsc::UnboundedReceiver<ElicitationCall>,
    prompt: &str,
    attachments: &[ContentBlock],
    queue: &mut VecDeque<QueuedOperation>,
    history: &localpilot_store::PromptHistory,
    cwd: &Path,
    mouse_state: &mut MouseState,
    workspace_index: &mut WorkspaceFileIndex,
) -> Result<bool> {
    let (events, mut rx) = broadcast::channel::<RuntimeEvent>(1024);
    let cancel = CancellationToken::new();
    // Snapshot immediately before the turn borrows the runtime. Mid-turn image
    // paste can then use the exact active provider without racing a model switch
    // or attempting a second mutable borrow for capability discovery.
    let image_capability = ImageCapabilitySnapshot {
        provider_id: runtime.active_provider_id().to_string(),
        vision_capable: runtime.active_accepts_images(),
    };
    let mut pending: Option<oneshot::Sender<bool>> = None;
    let mut pending_elicitation: Option<oneshot::Sender<ElicitationOutcome>> = None;
    let outcome = {
        let operation = runtime.run_turn_with_attachments(prompt, attachments, &events, &cancel);
        tokio::pin!(operation);
        let mut tick = tokio::time::interval(EVENT_POLL_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        async {
            loop {
        tokio::select! {
            biased;
            _ = tick.tick() => {
                workspace_index.refresh(app);
                let mut hit_map = draw_synchronized(terminal, app)?;
                for _ in 0..64 {
                    if !event::poll(Duration::ZERO).context("poll full-screen turn input")? {
                        break;
                    }
                    let next = event::read().context("read full-screen turn input")?;
                    let geometry_event = matches!(next, Event::Mouse(_) | Event::Resize(_, _));
                    let exit = if pending_elicitation.is_some() {
                        handle_question_event(
                            app,
                            next,
                            &mut pending_elicitation,
                            &cancel,
                            &hit_map,
                        )
                    } else if pending.is_some() {
                        handle_approval_event(app, next, &mut pending, &cancel)
                    } else {
                        match route_pointer_or_navigation(app, &next, &hit_map, mouse_state) {
                            RoutedEvent::Handled => false,
                            RoutedEvent::Copy(text) => {
                                copy_to_clipboard(app, text);
                                false
                            }
                            RoutedEvent::Unhandled => handle_turn_event(
                                app,
                                next,
                                &cancel,
                                &hit_map,
                                queue,
                                history,
                                cwd,
                                &image_capability,
                            ),
                        }
                    };
                    if exit {
                        cancel.cancel();
                        return Ok(true);
                    }
                    if geometry_event {
                        hit_map = draw_synchronized(terminal, app)?;
                    }
                }
                advance_mouse_selection(app, &hit_map, mouse_state);
                let _ = draw_synchronized(terminal, app)?;
            }
            reason = &mut operation => {
                drain_runtime_events(app, &mut rx);
                app.apply_runtime(map_runtime_event(RuntimeEvent::Stopped(reason)));
                let _ = draw_synchronized(terminal, app)?;
                return Ok(false);
            }
            Some(call) = approval_rx.recv(), if pending.is_none() && pending_elicitation.is_none() => {
                mouse_state.reset_gesture();
                app.request_approval(
                    call.request.tool,
                    call.request.target,
                    call.request.risk_class,
                );
                pending = Some(call.reply);
            }
            received = rx.recv() => {
                match received {
                    Ok(event) => app.apply_runtime(map_runtime_event(event)),
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => {}
                }
            }
            Some(call) = elicitation_rx.recv(), if pending.is_none() && pending_elicitation.is_none() => {
                mouse_state.reset_gesture();
                app.request_question(call.request.question, call.request.options);
                pending_elicitation = Some(call.reply);
            }
        }
            }
        }
        .await
    };
    deny_pending(app, &mut pending);
    deny_buffered_approvals(approval_rx);
    cancel_pending_elicitation(app, &mut pending_elicitation);
    cancel_buffered_elicitations(elicitation_rx);
    outcome
}

#[allow(clippy::too_many_arguments)] // the live input router threads each state owner explicitly
fn handle_turn_event(
    app: &mut AppModel,
    event: Event,
    cancel: &CancellationToken,
    hit_map: &HitMap,
    queue: &mut VecDeque<QueuedOperation>,
    history: &localpilot_store::PromptHistory,
    cwd: &Path,
    image_capability: &ImageCapabilitySnapshot,
) -> bool {
    match event {
        Event::Key(key) if is_key_action(key) => {
            if is_clipboard_image_key(key) {
                attach_clipboard_image_with_capability(app, image_capability, false);
                return false;
            }
            let Some(action) = map_key(key) else {
                return false;
            };
            match app.handle_input(action, hit_map.editor_width) {
                AppCommand::Exit => {
                    cancel.cancel();
                    true
                }
                AppCommand::CancelWork => {
                    cancel.cancel();
                    false
                }
                AppCommand::Copy(text) => {
                    copy_to_clipboard(app, text);
                    false
                }
                AppCommand::RunSlash(submitted) => {
                    if matches!(parse_slash(&submitted.prompt), Some(SlashAction::Quit)) {
                        cancel.cancel();
                        true
                    } else {
                        app.apply_runtime(RuntimeUpdate::Notice(
                            "slash commands run when the current operation is idle".to_string(),
                        ));
                        false
                    }
                }
                AppCommand::Submit(submitted) => {
                    if let Some(operation) =
                        prepare_prompt_operation(app, history, cwd, submitted, true)
                    {
                        queue.push_back(operation);
                    }
                    false
                }
                AppCommand::RunShell(command) => {
                    if let Some(operation) = prepare_shell_operation(app, command, true) {
                        queue.push_back(operation);
                    }
                    false
                }
                AppCommand::NavigateTakeover(navigation) => {
                    apply_takeover_navigation(app, navigation, hit_map);
                    false
                }
                AppCommand::NavigateTimeline(navigation) => {
                    apply_timeline_navigation(app, navigation, hit_map);
                    false
                }
                AppCommand::None | AppCommand::OpenExternalEditor => false,
            }
        }
        Event::Paste(text) => {
            if text.trim().is_empty() {
                attach_clipboard_image_with_capability(app, image_capability, true);
            } else {
                let _ = app.handle_input(InputAction::Paste(text), hit_map.editor_width);
            }
            false
        }
        Event::FocusGained
        | Event::FocusLost
        | Event::Mouse(_)
        | Event::Resize(_, _)
        | Event::Key(_) => false,
    }
}

fn route_pointer_or_navigation(
    app: &mut AppModel,
    event: &Event,
    hit_map: &HitMap,
    mouse_state: &mut MouseState,
) -> RoutedEvent {
    match event {
        Event::Mouse(mouse) => handle_mouse_event(app, *mouse, hit_map, mouse_state),
        Event::FocusLost => {
            mouse_state.reset_gesture();
            RoutedEvent::Handled
        }
        Event::Key(key) if is_key_action(*key) => {
            let Some(InputAction::NavigateTimeline(navigation)) = map_key(*key) else {
                return RoutedEvent::Unhandled;
            };
            let command = app.handle_input(
                InputAction::NavigateTimeline(navigation),
                hit_map.editor_width,
            );
            match command {
                AppCommand::NavigateTimeline(navigation) => {
                    apply_timeline_navigation(app, navigation, hit_map)
                }
                AppCommand::NavigateTakeover(navigation) => {
                    apply_takeover_navigation(app, navigation, hit_map)
                }
                _ => {}
            }
            RoutedEvent::Handled
        }
        Event::FocusGained | Event::Paste(_) | Event::Resize(_, _) | Event::Key(_) => {
            RoutedEvent::Unhandled
        }
    }
}

fn handle_mouse_event(
    app: &mut AppModel,
    mouse: MouseEvent,
    hit_map: &HitMap,
    mouse_state: &mut MouseState,
) -> RoutedEvent {
    if app.has_theme_picker() {
        app.disarm_exit();
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                let _ = app.handle_input(InputAction::MoveUp, hit_map.editor_width);
            }
            MouseEventKind::ScrollDown => {
                let _ = app.handle_input(InputAction::MoveDown, hit_map.editor_width);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(hit) = hit_map
                    .theme_rows
                    .iter()
                    .find(|hit| rect_contains(hit.area, mouse.column, mouse.row))
                {
                    app.select_theme(hit.index);
                }
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Moved
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => {}
        }
        return RoutedEvent::Handled;
    }
    if !matches!(
        mouse.kind,
        MouseEventKind::Moved | MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
    ) && app.dismiss_quick_help()
    {
        app.disarm_exit();
        mouse_state.reset_gesture();
        return RoutedEvent::Handled;
    }
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            app.disarm_exit();
            if hit_map.takeover {
                app.scroll_takeover_by(
                    -WHEEL_SCROLL_ROWS,
                    hit_map.scrollbar.total_rows,
                    hit_map.scrollbar.viewport_rows,
                );
            } else {
                app.timeline.scroll_by(
                    -WHEEL_SCROLL_ROWS,
                    hit_map.timeline_wrap_width,
                    hit_map.timeline.height,
                );
            }
            RoutedEvent::Handled
        }
        MouseEventKind::ScrollDown => {
            app.disarm_exit();
            if hit_map.takeover {
                app.scroll_takeover_by(
                    WHEEL_SCROLL_ROWS,
                    hit_map.scrollbar.total_rows,
                    hit_map.scrollbar.viewport_rows,
                );
            } else {
                app.timeline.scroll_by(
                    WHEEL_SCROLL_ROWS,
                    hit_map.timeline_wrap_width,
                    hit_map.timeline.height,
                );
            }
            RoutedEvent::Handled
        }
        MouseEventKind::Down(MouseButton::Left) => {
            app.disarm_exit();
            mouse_state.reset_gesture();

            if rect_contains(hit_map.scrollbar.track, mouse.column, mouse.row) {
                if let Some(thumb) = hit_map.scrollbar.thumb {
                    if rect_contains(thumb, mouse.column, mouse.row) {
                        mouse_state.scrollbar_grab = Some(mouse.row.saturating_sub(thumb.y));
                    } else {
                        let delta =
                            isize::try_from(hit_map.timeline.height.max(1)).unwrap_or(isize::MAX);
                        let delta = if mouse.row < thumb.y { -delta } else { delta };
                        if hit_map.takeover {
                            app.scroll_takeover_by(
                                delta,
                                hit_map.scrollbar.total_rows,
                                hit_map.scrollbar.viewport_rows,
                            );
                        } else {
                            app.timeline.scroll_by(
                                delta,
                                hit_map.timeline_wrap_width,
                                hit_map.timeline.height,
                            );
                        }
                    }
                }
                return RoutedEvent::Handled;
            }

            if hit_map.takeover {
                if let Some(hit) = hit_map
                    .takeover_file_rows
                    .iter()
                    .find(|hit| rect_contains(hit.area, mouse.column, mouse.row))
                {
                    app.select_diff_file(hit.index);
                } else if let Some(hit) = hit_map
                    .takeover_rows
                    .iter()
                    .find(|hit| rect_contains(hit.area, mouse.column, mouse.row))
                {
                    app.select_takeover_row(hit.index);
                }
                return RoutedEvent::Handled;
            }

            if let Some(tab) = hit_map
                .tabs
                .iter()
                .find(|tab| rect_contains(tab.area, mouse.column, mouse.row))
            {
                app.active_tab = tab.tab;
                app.timeline.clear_selection();
                return RoutedEvent::Handled;
            }

            if let Some(hit) = hit_map.timeline_rows.iter().find(|hit| {
                hit.y == mouse.row
                    && mouse.column >= hit_map.timeline.x
                    && mouse.column < hit.content_x
                    && matches!(hit.row.part, VisualRowPart::Content { first: true, .. })
            }) {
                if app.timeline.toggle_expandable(hit.row.item_id) {
                    app.timeline.clear_selection();
                    return RoutedEvent::Handled;
                }
            }

            if rect_contains(hit_map.composer, mouse.column, mouse.row) {
                if app.has_input_overlay() {
                    return RoutedEvent::Handled;
                }
                let visual_row = hit_map
                    .composer_scroll
                    .saturating_add(usize::from(mouse.row.saturating_sub(hit_map.composer.y)));
                app.editor.set_cursor_from_visual(
                    visual_row,
                    mouse.column.saturating_sub(hit_map.composer.x),
                    hit_map.editor_width,
                );
                app.timeline.clear_selection();
                return RoutedEvent::Handled;
            }

            if let Some((leading, trailing)) = selection_points(hit_map, mouse.column, mouse.row) {
                app.timeline.start_selection(leading);
                mouse_state.selection = Some(SelectionGesture {
                    leading,
                    trailing,
                    origin_column: mouse.column,
                    origin_row: mouse.row,
                });
                mouse_state.selection_pointer = Some((mouse.column, mouse.row));
            } else {
                app.timeline.clear_selection();
            }
            RoutedEvent::Handled
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            app.disarm_exit();
            if let Some(grab) = mouse_state.scrollbar_grab {
                let thumb_top = mouse.row.saturating_sub(grab);
                if let Some(start) = hit_map.scrollbar.content_start_for_thumb_top(thumb_top) {
                    if hit_map.takeover {
                        app.scroll_takeover_to(
                            start,
                            hit_map.scrollbar.total_rows,
                            hit_map.scrollbar.viewport_rows,
                        );
                    } else {
                        app.timeline.scroll_to_row(
                            start,
                            hit_map.timeline_wrap_width,
                            hit_map.timeline.height,
                        );
                    }
                }
                return RoutedEvent::Handled;
            }
            if hit_map.takeover {
                return RoutedEvent::Handled;
            }
            if mouse_state.selection.is_some() {
                mouse_state.selection_pointer = Some((mouse.column, mouse.row));
                advance_mouse_selection(app, hit_map, mouse_state);
            }
            RoutedEvent::Handled
        }
        MouseEventKind::Up(MouseButton::Left) => {
            app.disarm_exit();
            if mouse_state.scrollbar_grab.is_some() {
                mouse_state.reset_gesture();
                return RoutedEvent::Handled;
            }
            let selecting = mouse_state.selection;
            if let Some(gesture) = selecting {
                if (mouse.row, mouse.column) == (gesture.origin_row, gesture.origin_column) {
                    app.timeline.clear_selection();
                } else {
                    extend_mouse_selection(app, hit_map, mouse_state, mouse.column, mouse.row);
                }
            }
            mouse_state.reset_gesture();
            if selecting.is_some() && app.copy_on_select() {
                app.timeline
                    .selected_text()
                    .map_or(RoutedEvent::Handled, RoutedEvent::Copy)
            } else {
                RoutedEvent::Handled
            }
        }
        MouseEventKind::Down(MouseButton::Right) => {
            app.disarm_exit();
            if hit_map.takeover {
                return RoutedEvent::Handled;
            }
            app.timeline
                .selected_text()
                .map_or(RoutedEvent::Handled, RoutedEvent::Copy)
        }
        MouseEventKind::Moved => {
            mouse_state.reset_gesture();
            RoutedEvent::Handled
        }
        MouseEventKind::Down(_)
        | MouseEventKind::Up(_)
        | MouseEventKind::Drag(_)
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => {
            if hit_map.takeover {
                RoutedEvent::Handled
            } else {
                RoutedEvent::Unhandled
            }
        }
    }
}

fn advance_mouse_selection(app: &mut AppModel, hit_map: &HitMap, mouse_state: &MouseState) {
    let Some((column, row)) = mouse_state.selection_pointer else {
        return;
    };
    if row < hit_map.timeline.y {
        app.timeline
            .scroll_by(-1, hit_map.timeline_wrap_width, hit_map.timeline.height);
    } else if row >= hit_map.timeline.bottom() {
        app.timeline
            .scroll_by(1, hit_map.timeline_wrap_width, hit_map.timeline.height);
    }
    extend_mouse_selection(app, hit_map, mouse_state, column, row);
}

fn extend_mouse_selection(
    app: &mut AppModel,
    hit_map: &HitMap,
    mouse_state: &MouseState,
    column: u16,
    row: u16,
) {
    let Some(gesture) = mouse_state.selection else {
        return;
    };
    let Some((leading, trailing)) = selection_points_nearest(hit_map, column, row) else {
        return;
    };
    if (row, column) >= (gesture.origin_row, gesture.origin_column) {
        app.timeline.start_selection(gesture.leading);
        app.timeline.extend_selection(trailing);
    } else {
        app.timeline.start_selection(gesture.trailing);
        app.timeline.extend_selection(leading);
    }
}

fn selection_points(
    hit_map: &HitMap,
    column: u16,
    row: u16,
) -> Option<(ContentPoint, ContentPoint)> {
    let hit = hit_map.timeline_rows.iter().find(|hit| hit.y == row)?;
    Some((
        hit.point_for_column(column, false),
        hit.point_for_column(column, true),
    ))
}

fn selection_points_nearest(
    hit_map: &HitMap,
    column: u16,
    row: u16,
) -> Option<(ContentPoint, ContentPoint)> {
    selection_points(hit_map, column, row).or_else(|| {
        let hit = hit_map
            .timeline_rows
            .iter()
            .min_by_key(|hit| hit.y.abs_diff(row))?;
        Some((
            hit.point_for_column(column, false),
            hit.point_for_column(column, true),
        ))
    })
}

fn apply_timeline_navigation(app: &mut AppModel, navigation: TimelineNavigation, hit_map: &HitMap) {
    if hit_map.takeover {
        let navigation = match navigation {
            TimelineNavigation::PageUp => TakeoverNavigation::PageUp,
            TimelineNavigation::PageDown => TakeoverNavigation::PageDown,
        };
        apply_takeover_navigation(app, navigation, hit_map);
        return;
    }
    let page = isize::try_from(hit_map.timeline.height.max(1)).unwrap_or(isize::MAX);
    match navigation {
        TimelineNavigation::PageUp => {
            app.timeline
                .scroll_by(-page, hit_map.timeline_wrap_width, hit_map.timeline.height)
        }
        TimelineNavigation::PageDown => {
            app.timeline
                .scroll_by(page, hit_map.timeline_wrap_width, hit_map.timeline.height)
        }
    }
}

fn apply_takeover_navigation(app: &mut AppModel, navigation: TakeoverNavigation, hit_map: &HitMap) {
    let page = isize::try_from(hit_map.scrollbar.viewport_rows.max(1)).unwrap_or(isize::MAX);
    let delta = match navigation {
        TakeoverNavigation::LineUp => -1,
        TakeoverNavigation::LineDown => 1,
        TakeoverNavigation::PageUp => -page,
        TakeoverNavigation::PageDown => page,
    };
    app.scroll_takeover_by(
        delta,
        hit_map.scrollbar.total_rows,
        hit_map.scrollbar.viewport_rows,
    );
}

fn rect_contains(rect: ratatui::layout::Rect, column: u16, row: u16) -> bool {
    column >= rect.x && column < rect.right() && row >= rect.y && row < rect.bottom()
}

fn persist_prompt(
    app: &mut AppModel,
    history: &localpilot_store::PromptHistory,
    cwd: &Path,
    submitted: &SubmittedInput,
) {
    let pastes = submitted
        .pastes
        .iter()
        .map(|paste| localpilot_store::HistoryPaste {
            placeholder: paste.placeholder.clone(),
            content: paste.content.clone(),
        })
        .collect::<Vec<_>>();
    if let Err(error) = history.append(&submitted.shown, &pastes, cwd) {
        app.apply_runtime(RuntimeUpdate::Warning(format!(
            "prompt history could not be saved: {error}"
        )));
    }
}

fn expand_history_entry(entry: &localpilot_store::HistoryEntry) -> String {
    let mut expanded = String::with_capacity(entry.text.len());
    let mut copied = 0;
    for paste in &entry.pastes {
        let Some(relative) = entry.text[copied..].find(&paste.placeholder) else {
            continue;
        };
        let start = copied + relative;
        let end = start + paste.placeholder.len();
        expanded.push_str(&entry.text[copied..start]);
        expanded.push_str(&paste.content);
        copied = end;
    }
    expanded.push_str(&entry.text[copied..]);
    expanded
}

fn handle_approval_event(
    app: &mut AppModel,
    event: Event,
    pending: &mut Option<oneshot::Sender<bool>>,
    cancel: &CancellationToken,
) -> bool {
    let Event::Key(key) = event else {
        return false;
    };
    if !is_key_action(key) {
        return false;
    }
    if is_cancel(key) {
        let command = app.handle_input(InputAction::CancelOrExit, 1);
        return match command {
            AppCommand::Copy(text) => {
                copy_to_clipboard(app, text);
                false
            }
            AppCommand::Exit => {
                deny_pending(app, pending);
                cancel.cancel();
                true
            }
            AppCommand::CancelWork => {
                deny_pending(app, pending);
                cancel.cancel();
                false
            }
            _ => false,
        };
    }
    let answer = match key.code {
        KeyCode::Char('y' | 'Y') => Some(true),
        KeyCode::Enter if !app.capabilities.screen_reader => Some(true),
        KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(false),
        _ => None,
    };
    if let Some(answer) = answer {
        if let Some(reply) = pending.take() {
            let _ = reply.send(answer);
        }
        app.clear_dialog();
    }
    false
}

fn handle_question_event(
    app: &mut AppModel,
    event: Event,
    pending: &mut Option<oneshot::Sender<ElicitationOutcome>>,
    cancel: &CancellationToken,
    hit_map: &HitMap,
) -> bool {
    match event {
        Event::Mouse(mouse) => {
            app.disarm_exit();
            match mouse.kind {
                MouseEventKind::ScrollUp => app.timeline.scroll_by(
                    -WHEEL_SCROLL_ROWS,
                    hit_map.timeline_wrap_width,
                    hit_map.timeline.height,
                ),
                MouseEventKind::ScrollDown => app.timeline.scroll_by(
                    WHEEL_SCROLL_ROWS,
                    hit_map.timeline_wrap_width,
                    hit_map.timeline.height,
                ),
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(hit) = hit_map
                        .question_rows
                        .iter()
                        .find(|hit| rect_contains(hit.area, mouse.column, mouse.row))
                    {
                        app.select_question_option(hit.index);
                    }
                }
                _ => {}
            }
            false
        }
        Event::Paste(text) => {
            app.disarm_exit();
            let resolution = app.handle_question_input(InputAction::Paste(text));
            resolve_question_action(app, resolution, pending)
        }
        Event::Key(key) if is_key_action(key) => {
            if is_cancel(key) {
                return match app.handle_input(InputAction::CancelOrExit, hit_map.editor_width) {
                    AppCommand::Exit => {
                        finish_question(app, pending, ElicitationOutcome::Cancelled);
                        cancel.cancel();
                        true
                    }
                    AppCommand::CancelWork => {
                        finish_question(app, pending, ElicitationOutcome::Cancelled);
                        cancel.cancel();
                        false
                    }
                    AppCommand::Copy(text) => {
                        copy_to_clipboard(app, text);
                        false
                    }
                    _ => false,
                };
            }
            app.disarm_exit();
            let Some(action) = map_key(key) else {
                return false;
            };
            if let InputAction::NavigateTimeline(navigation) = action {
                apply_timeline_navigation(app, navigation, hit_map);
                return false;
            }
            let resolution = app.handle_question_input(action);
            resolve_question_action(app, resolution, pending)
        }
        Event::FocusGained | Event::FocusLost | Event::Resize(_, _) | Event::Key(_) => false,
    }
}

fn resolve_question_action(
    app: &mut AppModel,
    action: QuestionAction,
    pending: &mut Option<oneshot::Sender<ElicitationOutcome>>,
) -> bool {
    match action {
        QuestionAction::None => false,
        QuestionAction::Submit(answer) => {
            finish_question(app, pending, ElicitationOutcome::Answered(answer));
            false
        }
        QuestionAction::Cancel => {
            finish_question(app, pending, ElicitationOutcome::Cancelled);
            false
        }
    }
}

fn finish_question(
    app: &mut AppModel,
    pending: &mut Option<oneshot::Sender<ElicitationOutcome>>,
    outcome: ElicitationOutcome,
) {
    if let Some(reply) = pending.take() {
        let _ = reply.send(outcome);
    }
    app.clear_dialog();
}

fn deny_pending(app: &mut AppModel, pending: &mut Option<oneshot::Sender<bool>>) {
    if let Some(reply) = pending.take() {
        let _ = reply.send(false);
    }
    app.clear_dialog();
}

fn deny_buffered_approvals(approval_rx: &mut mpsc::UnboundedReceiver<ApprovalCall>) {
    while let Ok(call) = approval_rx.try_recv() {
        let _ = call.reply.send(false);
    }
}

fn cancel_pending_elicitation(
    app: &mut AppModel,
    pending: &mut Option<oneshot::Sender<ElicitationOutcome>>,
) {
    finish_question(app, pending, ElicitationOutcome::Cancelled);
}

fn cancel_buffered_elicitations(elicitation_rx: &mut mpsc::UnboundedReceiver<ElicitationCall>) {
    while let Ok(call) = elicitation_rx.try_recv() {
        let _ = call.reply.send(ElicitationOutcome::Cancelled);
    }
}

fn present_shell_diagnostic(output: &str) -> &str {
    output
        .split_once("\noutput:\n")
        .map_or(output, |(_, body)| body)
        .trim()
}

fn drain_runtime_events(app: &mut AppModel, rx: &mut broadcast::Receiver<RuntimeEvent>) {
    loop {
        match rx.try_recv() {
            Ok(event) => app.apply_runtime(map_runtime_event(event)),
            Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
            Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed) => {
                break
            }
        }
    }
}

fn local_prompt_time() -> String {
    let offset = LOCAL_UTC_OFFSET
        .get()
        .copied()
        .unwrap_or(time::UtcOffset::UTC);
    let now = time::OffsetDateTime::now_utc().to_offset(offset);
    format_prompt_time(now)
}

fn format_prompt_time(now: time::OffsetDateTime) -> String {
    format!("{:02}:{:02}", now.hour(), now.minute())
}

fn copy_to_clipboard(app: &mut AppModel, text: String) {
    let result = arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text));
    if let Err(error) = result {
        app.apply_runtime(RuntimeUpdate::Warning(format!(
            "clipboard copy unavailable: {error}"
        )));
    }
}

fn draw_synchronized(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &AppModel,
) -> Result<localpilot_terminal_ui::HitMap> {
    execute!(terminal.backend_mut(), BeginSynchronizedUpdate)
        .context("begin synchronized full-screen update")?;
    let mut hit_map = None;
    let draw_result = terminal
        .draw(|frame| hit_map = Some(render(frame, app)))
        .map(|_| ());
    let end_result = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
    draw_result.context("draw full-screen frame")?;
    end_result.context("end synchronized full-screen update")?;
    hit_map.context("full-screen render did not produce a hit map")
}

fn map_key(key: KeyEvent) -> Option<InputAction> {
    if is_cancel(key) {
        return Some(InputAction::CancelOrExit);
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    match key.code {
        KeyCode::PageUp => Some(InputAction::NavigateTimeline(TimelineNavigation::PageUp)),
        KeyCode::PageDown => Some(InputAction::NavigateTimeline(TimelineNavigation::PageDown)),
        KeyCode::Home if ctrl && !alt => Some(InputAction::MoveTextStart),
        KeyCode::End if ctrl && !alt => Some(InputAction::MoveTextEnd),
        KeyCode::Home if !ctrl && !alt => Some(InputAction::MoveVisualStart),
        KeyCode::End if !ctrl && !alt => Some(InputAction::MoveVisualEnd),
        KeyCode::Left if alt && !ctrl => Some(InputAction::MoveWordLeft),
        KeyCode::Right if alt && !ctrl => Some(InputAction::MoveWordRight),
        KeyCode::Char('a') if ctrl && !alt => Some(InputAction::MoveLineStart),
        KeyCode::Char('b') if ctrl && !alt => Some(InputAction::MoveLeft),
        KeyCode::Char('e') if ctrl && !alt => Some(InputAction::MoveLineEnd),
        KeyCode::Char('f') if ctrl && !alt => Some(InputAction::ForwardCharOrSearch),
        KeyCode::Char('g') if ctrl && !alt => Some(InputAction::OpenExternalEditor),
        KeyCode::Char('h') if ctrl && !alt => Some(InputAction::Backspace),
        KeyCode::Char('j') if ctrl && !alt => Some(InputAction::Insert("\n".to_string())),
        KeyCode::Char('k') if ctrl && !alt => Some(InputAction::DeleteToLineEnd),
        KeyCode::Char('r') if ctrl && !alt => Some(InputAction::OpenReverseHistory),
        KeyCode::Char('u') if ctrl && !alt => Some(InputAction::DeleteToLineStart),
        KeyCode::Char('w') if ctrl && !alt => Some(InputAction::DeleteWordLeft),
        KeyCode::Char('y') if ctrl && !alt => Some(InputAction::AcceptCompletion),
        KeyCode::Char(character) if !ctrl && !alt => {
            Some(InputAction::Insert(character.to_string()))
        }
        KeyCode::Enter if alt || shift => Some(InputAction::Insert("\n".to_string())),
        KeyCode::Enter => Some(InputAction::Submit),
        KeyCode::Tab => Some(InputAction::AcceptCompletion),
        KeyCode::Esc => Some(InputAction::Escape),
        KeyCode::Backspace => Some(InputAction::Backspace),
        KeyCode::Delete => Some(InputAction::Delete),
        KeyCode::Left => Some(InputAction::MoveLeft),
        KeyCode::Right => Some(InputAction::MoveRight),
        KeyCode::Up => Some(InputAction::MoveUp),
        KeyCode::Down => Some(InputAction::MoveDown),
        _ => None,
    }
}

pub(crate) fn map_runtime_event(event: RuntimeEvent) -> RuntimeUpdate {
    match event {
        RuntimeEvent::Text(text) => RuntimeUpdate::Text(text),
        RuntimeEvent::Reasoning(text) => RuntimeUpdate::Reasoning(text),
        RuntimeEvent::ToolStarted { id, name, detail } => {
            RuntimeUpdate::ToolStarted { id, name, detail }
        }
        RuntimeEvent::ToolFinished {
            id,
            name,
            is_error,
            cancelled,
            output,
            duration_ms,
        } => RuntimeUpdate::ToolFinished {
            id,
            name,
            is_error,
            cancelled,
            output,
            duration_ms,
        },
        RuntimeEvent::Usage(usage) => RuntimeUpdate::Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
        },
        RuntimeEvent::ContextUsage { used, limit } => RuntimeUpdate::ContextUsage { used, limit },
        RuntimeEvent::Warning(message) => RuntimeUpdate::Warning(message),
        RuntimeEvent::Plan(steps) => RuntimeUpdate::Plan(
            steps
                .into_iter()
                .map(|step| PlanEntry {
                    title: step.title,
                    status: step.status,
                })
                .collect(),
        ),
        RuntimeEvent::QuotaPaused { reset } => RuntimeUpdate::QuotaPaused(reset),
        RuntimeEvent::Recovery { health } => RuntimeUpdate::Recovery(match health {
            ModelHealth::Healthy => RecoveryState::Healthy,
            ModelHealth::Recovering => RecoveryState::Recovering,
            ModelHealth::Degraded => RecoveryState::Degraded,
        }),
        RuntimeEvent::ToolStuck { name, count } => RuntimeUpdate::ToolStuck { name, count },
        RuntimeEvent::Stopped(reason) => RuntimeUpdate::Stopped(match reason {
            StopReason::Done => StopState::Done,
            StopReason::Cancelled => StopState::Cancelled,
            StopReason::Degraded => StopState::Degraded,
            StopReason::ProviderError => StopState::ProviderError,
            StopReason::BudgetExceeded => StopState::BudgetExceeded,
            StopReason::NoProgress => StopState::NoProgress,
            StopReason::TimedOut => StopState::TimedOut,
        }),
    }
}

struct TerminalModes {
    active: bool,
    mouse_capture: bool,
}

impl TerminalModes {
    fn enter(mouse_capture: bool) -> Result<(Self, TerminalCapabilities)> {
        terminal::enable_raw_mode().context("enable raw terminal mode")?;
        TERMINAL_MODES_ACTIVE.store(true, Ordering::Release);
        MOUSE_CAPTURE_ACTIVE.store(mouse_capture, Ordering::Release);
        let mut guard = Self {
            active: true,
            mouse_capture,
        };
        let mut stdout = io::stdout();
        if let Err(error) = write_required_modes(&mut stdout, mouse_capture) {
            guard.restore();
            return Err(error).context("enter full-screen terminal modes");
        }
        let enhanced = write_keyboard_enhancement(&mut stdout).is_ok();
        KEYBOARD_FLAGS_PUSHED.store(enhanced, Ordering::Release);
        let clipboard_write = arboard::Clipboard::new().is_ok();
        let capabilities = TerminalCapabilities {
            color: if std::env::var_os("NO_COLOR").is_some() {
                ColorSupport::NoColor
            } else {
                ColorSupport::Color
            },
            mouse_capture,
            synchronized_output: true,
            keyboard: if enhanced {
                KeyboardSupport::Enhanced
            } else {
                KeyboardSupport::Basic
            },
            clipboard_write,
            screen_reader: std::env::var(CHAT_SCREEN_READER_ENV)
                .ok()
                .as_deref()
                .and_then(parse_bool_setting)
                .unwrap_or(false),
        };
        Ok((guard, capabilities))
    }

    fn restore(&mut self) {
        if !self.active {
            return;
        }
        restore_terminal_modes();
        self.active = false;
    }
}

impl SuspensibleModes for TerminalModes {
    type Capabilities = TerminalCapabilities;

    fn leave(&mut self) {
        self.restore();
    }

    fn reenter(&mut self) -> Result<Self::Capabilities> {
        let (replacement, capabilities) = Self::enter(self.mouse_capture)?;
        *self = replacement;
        Ok(capabilities)
    }
}

impl Drop for TerminalModes {
    fn drop(&mut self) {
        self.restore();
    }
}

fn write_required_modes(writer: &mut impl Write, mouse_capture: bool) -> io::Result<()> {
    execute!(writer, EnterAlternateScreen)?;
    if mouse_capture {
        execute!(writer, EnableMouseCapture)?;
    }
    execute!(writer, EnableBracketedPaste, Hide)
}

fn write_keyboard_enhancement(writer: &mut impl Write) -> io::Result<()> {
    execute!(
        writer,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES,
        )
    )
}

fn write_restore_modes(
    writer: &mut impl Write,
    keyboard_flags_pushed: bool,
    mouse_capture: bool,
) -> io::Result<()> {
    if keyboard_flags_pushed {
        // Keyboard enhancement is opportunistic. Its legacy Windows command
        // may be unsupported, but that must never prevent required cleanup.
        let _ = execute!(writer, PopKeyboardEnhancementFlags);
    }
    execute!(writer, Show, DisableBracketedPaste)?;
    if mouse_capture {
        execute!(writer, DisableMouseCapture)?;
    }
    execute!(writer, LeaveAlternateScreen)
}

fn restore_terminal_modes() {
    if !TERMINAL_MODES_ACTIVE.swap(false, Ordering::AcqRel) {
        return;
    }
    let keyboard_flags_pushed = KEYBOARD_FLAGS_PUSHED.swap(false, Ordering::AcqRel);
    let mouse_capture = MOUSE_CAPTURE_ACTIVE.swap(false, Ordering::AcqRel);
    let _ = write_restore_modes(&mut io::stdout(), keyboard_flags_pushed, mouse_capture);
    let _ = terminal::disable_raw_mode();
}

fn install_panic_restore_hook() {
    let driver = std::thread::current().id();
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == driver {
            restore_terminal_modes();
        }
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState, MouseEvent};
    use localpilot_core::TokenUsage;
    use localpilot_terminal_ui::{ItemKind, ViewportAnchor, WorkState};
    use ratatui::backend::TestBackend;

    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn draw_hit_map(app: &AppModel, width: u16, height: u16) -> HitMap {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, app)))
            .expect("draw hit map");
        hit_map.expect("hit map")
    }

    fn event_hit_map() -> HitMap {
        draw_hit_map(&app(), 80, 24)
    }

    fn app() -> AppModel {
        AppModel::new(
            Header {
                version: "0".to_string(),
                provider: "fixture".to_string(),
                model: "fixture-model".to_string(),
                workspace: "fixture-workspace".to_string(),
                branch: Some("fixture-branch".to_string()),
                workspace_dirty: Some(true),
                mode: "agent".to_string(),
                profile: "default".to_string(),
                session_id: "fixture-session".to_string(),
                session_name: None,
            },
            TerminalCapabilities::default(),
        )
    }

    fn image_capability(vision_capable: bool) -> ImageCapabilitySnapshot {
        ImageCapabilitySnapshot {
            provider_id: "fixture".to_string(),
            vision_capable,
        }
    }

    #[test]
    fn ctrl_c_maps_to_contextual_interrupt_handling() {
        assert_eq!(
            map_key(press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(InputAction::CancelOrExit)
        );
    }

    #[test]
    fn enter_submits_and_escape_maps_to_work_interrupt() {
        assert_eq!(
            map_key(press(KeyCode::Enter, KeyModifiers::NONE)),
            Some(InputAction::Submit)
        );
        assert_eq!(
            map_key(press(KeyCode::Esc, KeyModifiers::NONE)),
            Some(InputAction::Escape)
        );
        assert_eq!(
            map_key(press(KeyCode::Enter, KeyModifiers::SHIFT)),
            Some(InputAction::Insert("\n".to_string()))
        );
    }

    #[test]
    fn wheel_and_page_navigation_hold_idle_and_busy_timelines() {
        let mut app = app();
        for number in 0..100 {
            let _ = app
                .timeline
                .push(ItemKind::Assistant, format!("response {number:03}"));
        }
        let hit_map = draw_hit_map(&app, 80, 24);
        let mut mouse_state = MouseState::default();
        app.exit_armed = true;
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::ScrollUp,
                    hit_map.timeline.x,
                    hit_map.timeline.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert!(matches!(
            app.timeline.viewport,
            ViewportAnchor::Held(_) | ViewportAnchor::Top
        ));
        assert!(!app.exit_armed);

        app.timeline.follow_bottom();
        app.begin_work();
        let busy_hit_map = draw_hit_map(&app, 80, 24);
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::ScrollUp,
                    busy_hit_map.timeline.x,
                    busy_hit_map.timeline.y,
                )),
                &busy_hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert!(matches!(
            app.timeline.viewport,
            ViewportAnchor::Held(_) | ViewportAnchor::Top
        ));

        app.timeline.follow_bottom();
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Key(press(KeyCode::PageUp, KeyModifiers::NONE)),
                &busy_hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert!(matches!(
            app.timeline.viewport,
            ViewportAnchor::Held(_) | ViewportAnchor::Top
        ));
        let held = app.timeline.viewport;
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Key(press(KeyCode::Home, KeyModifiers::CONTROL)),
                &busy_hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Unhandled
        );
        assert_eq!(app.timeline.viewport, held);
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Key(press(KeyCode::End, KeyModifiers::CONTROL)),
                &busy_hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Unhandled
        );
        assert_eq!(app.timeline.viewport, held);
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Key(press(KeyCode::Home, KeyModifiers::NONE)),
                &busy_hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Unhandled
        );
    }

    #[test]
    fn composer_navigation_and_shell_editing_shortcuts_map_semantically() {
        let cases = [
            (
                KeyCode::Home,
                KeyModifiers::NONE,
                InputAction::MoveVisualStart,
            ),
            (KeyCode::End, KeyModifiers::NONE, InputAction::MoveVisualEnd),
            (
                KeyCode::Home,
                KeyModifiers::CONTROL,
                InputAction::MoveTextStart,
            ),
            (
                KeyCode::End,
                KeyModifiers::CONTROL,
                InputAction::MoveTextEnd,
            ),
            (KeyCode::Left, KeyModifiers::ALT, InputAction::MoveWordLeft),
            (
                KeyCode::Right,
                KeyModifiers::ALT,
                InputAction::MoveWordRight,
            ),
            (
                KeyCode::Char('a'),
                KeyModifiers::CONTROL,
                InputAction::MoveLineStart,
            ),
            (
                KeyCode::Char('b'),
                KeyModifiers::CONTROL,
                InputAction::MoveLeft,
            ),
            (
                KeyCode::Char('e'),
                KeyModifiers::CONTROL,
                InputAction::MoveLineEnd,
            ),
            (
                KeyCode::Char('f'),
                KeyModifiers::CONTROL,
                InputAction::ForwardCharOrSearch,
            ),
            (
                KeyCode::Char('g'),
                KeyModifiers::CONTROL,
                InputAction::OpenExternalEditor,
            ),
            (
                KeyCode::Char('h'),
                KeyModifiers::CONTROL,
                InputAction::Backspace,
            ),
            (
                KeyCode::Char('k'),
                KeyModifiers::CONTROL,
                InputAction::DeleteToLineEnd,
            ),
            (
                KeyCode::Char('u'),
                KeyModifiers::CONTROL,
                InputAction::DeleteToLineStart,
            ),
            (
                KeyCode::Char('w'),
                KeyModifiers::CONTROL,
                InputAction::DeleteWordLeft,
            ),
            (
                KeyCode::Char('j'),
                KeyModifiers::CONTROL,
                InputAction::Insert("\n".to_string()),
            ),
            (
                KeyCode::Char('r'),
                KeyModifiers::CONTROL,
                InputAction::OpenReverseHistory,
            ),
            (
                KeyCode::Char('y'),
                KeyModifiers::CONTROL,
                InputAction::AcceptCompletion,
            ),
            (
                KeyCode::Tab,
                KeyModifiers::NONE,
                InputAction::AcceptCompletion,
            ),
        ];
        for (code, modifiers, expected) in cases {
            assert_eq!(map_key(press(code, modifiers)), Some(expected));
        }
    }

    #[test]
    fn fullscreen_catalog_exposes_only_commands_with_a_real_fullscreen_path() {
        assert!(!localpilot_tui::AppState::slash_commands()
            .iter()
            .any(|(name, _)| *name == "search"));
        let catalog = fullscreen_command_catalog();
        let search = catalog
            .iter()
            .filter(|command| command.name == "search")
            .collect::<Vec<_>>();
        assert_eq!(search.len(), 1);
        assert_eq!(search[0].description, "Search messages in this session");
        for supported in [
            "model", "new", "fork", "clone", "clear", "quit", "help", "theme", "settings", "diff",
        ] {
            assert!(catalog.iter().any(|command| command.name == supported));
        }
        for deferred in ["compact", "research", "skills", "sessions"] {
            assert!(!catalog.iter().any(|command| command.name == deferred));
        }
    }

    #[test]
    fn external_editor_resolution_honors_precedence_quotes_and_redacts_arguments() {
        let command = resolve_editor_command_with(|name| match name {
            CHAT_EDITOR_ENV => Some(OsString::from(
                "\"C:\\Program Files\\Editor\\editor.exe\" --wait --token SECRET_ARG",
            )),
            "VISUAL" => Some(OsString::from("ignored-visual")),
            "EDITOR" => Some(OsString::from("ignored-editor")),
            _ => None,
        })
        .expect("editor command");
        assert_eq!(
            command.program,
            OsString::from("C:\\Program Files\\Editor\\editor.exe")
        );
        assert_eq!(
            command.args,
            [
                OsString::from("--wait"),
                OsString::from("--token"),
                OsString::from("SECRET_ARG")
            ]
        );
        assert!(!format!("{command:?}").contains("SECRET_ARG"));
        assert!(split_editor_command("\"unterminated").is_err());
    }

    #[test]
    fn external_editor_readback_rejects_invalid_utf8_and_oversize_files() {
        let directory = tempfile::tempdir().expect("editor fixture");
        let path = directory.path().join("LOCALPILOT_PROMPT.md");
        std::fs::write(&path, b"edited draft").expect("valid fixture");
        assert_eq!(
            read_external_edit(&path).expect("valid UTF-8 fixture"),
            "edited draft"
        );

        std::fs::write(&path, [0xff, 0xfe]).expect("invalid fixture");
        assert!(read_external_edit(&path)
            .expect_err("invalid UTF-8")
            .to_string()
            .contains("not valid UTF-8"));

        let file = std::fs::File::create(&path).expect("oversize fixture");
        file.set_len(MAX_EXTERNAL_EDITOR_BYTES + 1)
            .expect("sparse oversize fixture");
        assert!(read_external_edit(&path)
            .expect_err("oversize file")
            .to_string()
            .contains("8 MiB"));
    }

    #[derive(Default)]
    struct FakeModes {
        events: Vec<&'static str>,
    }

    impl SuspensibleModes for FakeModes {
        type Capabilities = &'static str;

        fn leave(&mut self) {
            self.events.push("leave");
        }

        fn reenter(&mut self) -> Result<Self::Capabilities> {
            self.events.push("reenter");
            Ok("capabilities")
        }
    }

    #[tokio::test]
    async fn suspended_operation_reenters_after_success_and_operation_error() {
        let mut success_modes = FakeModes::default();
        let (value, capabilities) = with_modes_suspended(&mut success_modes, async { 42 })
            .await
            .expect("successful round trip");
        assert_eq!(value, 42);
        assert_eq!(capabilities, "capabilities");
        assert_eq!(success_modes.events, ["leave", "reenter"]);

        let mut failed_operation_modes = FakeModes::default();
        let (operation, _) = with_modes_suspended(&mut failed_operation_modes, async {
            Err::<(), _>("injected spawn failure")
        })
        .await
        .expect("terminal re-entry still succeeds");
        assert_eq!(operation, Err("injected spawn failure"));
        assert_eq!(failed_operation_modes.events, ["leave", "reenter"]);
    }

    #[test]
    fn suspended_operation_leaves_a_plain_terminal_during_panic_unwind() {
        let mut modes = FakeModes::default();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _suspension = ModeSuspension::new(&mut modes);
            panic!("injected editor panic");
        }));
        assert!(result.is_err());
        assert_eq!(modes.events, ["leave"]);
    }

    #[test]
    fn mouse_drag_selects_graphemes_and_copy_on_release_persists() {
        let mut app = app();
        app.set_copy_on_select(true);
        let _ = app.timeline.push(ItemKind::Assistant, "alpha 界 beta");
        let hit_map = draw_hit_map(&app, 80, 24);
        let hit = hit_map
            .timeline_rows
            .iter()
            .find(|hit| hit.row.kind == ItemKind::Assistant)
            .expect("assistant row hit");
        let start_column = hit.content_x;
        let end_column = hit.content_x + 6;
        let row = hit.y;
        let mut mouse_state = MouseState::default();

        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    start_column,
                    row
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Drag(MouseButton::Left),
                    end_column,
                    row
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(app.timeline.selected_text().as_deref(), Some("alpha 界"));
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Up(MouseButton::Left),
                    end_column,
                    row
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Copy("alpha 界".to_string())
        );
        assert_eq!(app.timeline.selected_text().as_deref(), Some("alpha 界"));
        assert!(mouse_state.selection.is_none());

        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    start_column,
                    row
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Up(MouseButton::Left),
                    start_column,
                    row
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert!(app.timeline.selected_text().is_none());
    }

    #[test]
    fn default_selection_waits_for_explicit_right_click_copy() {
        let mut app = app();
        let _ = app.timeline.push(ItemKind::Assistant, "copy explicitly");
        assert!(!app.copy_on_select());
        let hit_map = draw_hit_map(&app, 80, 24);
        let hit = hit_map
            .timeline_rows
            .iter()
            .find(|hit| hit.row.kind == ItemKind::Assistant)
            .expect("assistant row hit");
        let start = hit.content_x;
        let end = hit.content_x.saturating_add(3);
        let mut mouse_state = MouseState::default();
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
        ] {
            let column = if matches!(kind, MouseEventKind::Down(_)) {
                start
            } else {
                end
            };
            assert_eq!(
                route_pointer_or_navigation(
                    &mut app,
                    &Event::Mouse(mouse(kind, column, hit.y)),
                    &hit_map,
                    &mut mouse_state,
                ),
                RoutedEvent::Handled
            );
        }
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(MouseEventKind::Up(MouseButton::Left), end, hit.y)),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(app.timeline.selected_text().as_deref(), Some("copy"));
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(MouseEventKind::Down(MouseButton::Right), end, hit.y,)),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Copy("copy".to_string())
        );
    }

    #[test]
    fn lost_mouse_release_self_heals_on_focus_loss_or_unpressed_motion() {
        let mut app = app();
        let _ = app.timeline.push(ItemKind::Assistant, "select me");
        let hit_map = draw_hit_map(&app, 80, 24);
        let hit = hit_map
            .timeline_rows
            .iter()
            .find(|hit| hit.row.kind == ItemKind::Assistant)
            .expect("assistant row hit");
        let mut mouse_state = MouseState::default();

        for recovery in [
            Event::FocusLost,
            Event::Mouse(mouse(MouseEventKind::Moved, 0, 0)),
        ] {
            let _ = route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    hit.content_x,
                    hit.y,
                )),
                &hit_map,
                &mut mouse_state,
            );
            assert!(mouse_state.selection_pointer.is_some());
            assert_eq!(
                route_pointer_or_navigation(&mut app, &recovery, &hit_map, &mut mouse_state),
                RoutedEvent::Handled
            );
            assert!(mouse_state.selection.is_none());
            assert!(mouse_state.selection_pointer.is_none());
        }
    }

    #[test]
    fn quick_help_wheel_scrolls_the_timeline_without_consuming_the_first_step() {
        let mut app = app();
        for number in 0..80 {
            let _ = app
                .timeline
                .push(ItemKind::Assistant, format!("response {number:03}"));
        }
        let _ = app.handle_input(InputAction::Insert("?".to_string()), 76);
        let hit_map = draw_hit_map(&app, 80, 24);
        let mut mouse_state = MouseState::default();

        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(MouseEventKind::ScrollUp, 10, 8)),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert!(app.dismiss_quick_help());
        assert!(matches!(
            app.timeline.viewport,
            localpilot_terminal_ui::ViewportAnchor::Held(_)
        ));
    }

    #[test]
    fn held_edge_selection_continues_to_autoscroll_without_new_mouse_events() {
        let mut app = app();
        for number in 0..120 {
            let _ = app
                .timeline
                .push(ItemKind::Assistant, format!("response {number:03}"));
        }
        let hit_map = draw_hit_map(&app, 80, 24);
        let origin = hit_map.timeline_rows.last().expect("visible timeline row");
        let mut mouse_state = MouseState::default();
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    origin.content_x,
                    origin.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Drag(MouseButton::Left),
                    origin.content_x,
                    hit_map.timeline.y.saturating_sub(1),
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );

        let after_drag = draw_hit_map(&app, 80, 24);
        let start_after_drag = after_drag.scrollbar.start;
        advance_mouse_selection(&mut app, &after_drag, &mouse_state);
        let after_stationary_tick = draw_hit_map(&app, 80, 24);

        assert!(after_stationary_tick.scrollbar.start < start_after_drag);
        assert!(app.timeline.selected_text().is_some());
    }

    #[test]
    fn activity_prefix_click_toggles_details_without_starting_selection() {
        let mut app = app();
        app.apply_runtime(RuntimeUpdate::ToolStarted {
            id: "tool-1".to_string(),
            name: "inspect".to_string(),
            detail: String::new(),
        });
        app.apply_runtime(RuntimeUpdate::ToolFinished {
            id: "tool-1".to_string(),
            name: "inspect".to_string(),
            is_error: false,
            cancelled: false,
            output: "detail one\ndetail two".to_string(),
            duration_ms: 25,
        });
        let tool = app
            .timeline
            .items()
            .iter()
            .find(|item| item.kind == ItemKind::Tool)
            .expect("tool item")
            .id;
        assert!(!app.timeline.item(tool).expect("tool item").expanded);
        let hit_map = draw_hit_map(&app, 80, 24);
        let hit = hit_map
            .timeline_rows
            .iter()
            .find(|hit| hit.row.item_id == tool)
            .expect("tool row hit");
        let mut mouse_state = MouseState::default();

        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    hit_map.timeline.x,
                    hit.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert!(app.timeline.item(tool).expect("tool item").expanded);
        assert!(app.timeline.selection.is_none());
    }

    #[test]
    fn scrollbar_thumb_drag_and_track_click_reanchor_timeline() {
        let mut app = app();
        for number in 0..120 {
            let _ = app
                .timeline
                .push(ItemKind::Assistant, format!("response {number:03}"));
        }
        let hit_map = draw_hit_map(&app, 80, 24);
        let thumb = hit_map.scrollbar.thumb.expect("scrollbar thumb");
        let mut mouse_state = MouseState::default();

        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    thumb.x,
                    thumb.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(mouse_state.scrollbar_grab, Some(0));
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Drag(MouseButton::Left),
                    thumb.x,
                    hit_map.scrollbar.track.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(app.timeline.viewport, ViewportAnchor::Top);
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Up(MouseButton::Left),
                    thumb.x,
                    hit_map.scrollbar.track.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(mouse_state.scrollbar_grab, None);

        app.timeline.follow_bottom();
        let hit_map = draw_hit_map(&app, 80, 24);
        let thumb = hit_map.scrollbar.thumb.expect("bottom thumb");
        let click_y = thumb.y.saturating_sub(1);
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    thumb.x,
                    click_y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert!(matches!(
            app.timeline.viewport,
            ViewportAnchor::Held(_) | ViewportAnchor::Top
        ));
    }

    #[test]
    fn help_takeover_contains_mouse_input_and_scrolls_its_own_view() {
        let mut app = app();
        app.set_command_catalog(fullscreen_command_catalog());
        app.open_help();
        let mut hit_map = draw_hit_map(&app, 80, 20);
        assert!(hit_map.takeover);
        assert_eq!(hit_map.scrollbar.start, 0);
        let mut mouse_state = MouseState::default();

        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(MouseEventKind::ScrollDown, 20, 8)),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        hit_map = draw_hit_map(&app, 80, 20);
        assert!(hit_map.scrollbar.start > 0);

        let thumb = hit_map.scrollbar.thumb.expect("help scrollbar thumb");
        let track_bottom = hit_map.scrollbar.track.bottom().saturating_sub(1);
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    thumb.x,
                    thumb.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Drag(MouseButton::Left),
                    thumb.x,
                    track_bottom,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        hit_map = draw_hit_map(&app, 80, 20);
        assert_eq!(
            hit_map.scrollbar.start,
            hit_map
                .scrollbar
                .total_rows
                .saturating_sub(hit_map.scrollbar.viewport_rows)
        );
        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(MouseEventKind::Down(MouseButton::Right), 20, 8)),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
    }

    #[test]
    fn theme_picker_mouse_focus_previews_without_touching_the_timeline() {
        let mut app = app();
        let timeline_item = app
            .timeline
            .push(ItemKind::Assistant, "underlying conversation")
            .expect("timeline item");
        app.open_theme_picker();
        let hit_map = draw_hit_map(&app, 80, 24);
        assert_eq!(hit_map.theme_rows.len(), Theme::ALL.len());
        let dim = hit_map.theme_rows[1];
        let mut mouse_state = MouseState::default();

        assert_eq!(
            route_pointer_or_navigation(
                &mut app,
                &Event::Mouse(mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    dim.area.x,
                    dim.area.y,
                )),
                &hit_map,
                &mut mouse_state,
            ),
            RoutedEvent::Handled
        );
        assert_eq!(app.theme, Theme::Dim);
        assert!(app.timeline.item(timeline_item).is_some());
        assert!(app.has_theme_picker());
        let _ = app.handle_input(InputAction::Escape, hit_map.editor_width);
        assert_eq!(app.theme, Theme::Default);
        assert!(!app.has_theme_picker());
    }

    #[test]
    fn active_turn_queues_typeahead_and_escape_cancels_real_token() {
        let mut app = app();
        app.begin_work();
        app.editor.insert("next prompt");
        let cancel = CancellationToken::new();
        let history = localpilot_store::PromptHistory::with_store(None);
        let cwd = Path::new("fixture");
        let mut queue = VecDeque::new();

        assert!(!handle_turn_event(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            cwd,
            &image_capability(false),
        ));
        assert!(app.editor.text().is_empty());
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.front().expect("queued").prompt().text, "next prompt");
        assert!(
            app.timeline
                .item(queue.front().expect("queued").item_id())
                .expect("queued item")
                .pending
        );
        assert!(!cancel.is_cancelled());

        app.editor.insert("third prompt");
        assert!(!handle_turn_event(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            cwd,
            &image_capability(false),
        ));
        assert_eq!(
            queue
                .iter()
                .map(|queued| queued.prompt().text.as_str())
                .collect::<Vec<_>>(),
            vec!["next prompt", "third prompt"]
        );

        assert!(!handle_turn_event(
            &mut app,
            Event::Key(press(KeyCode::Esc, KeyModifiers::NONE)),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            cwd,
            &image_capability(false),
        ));
        assert!(cancel.is_cancelled());
        assert_eq!(
            app.work,
            WorkState::Busy {
                cancellation_requested: true
            }
        );
    }

    #[test]
    fn active_turn_queues_shell_then_prompt_in_one_serial_order() {
        let temp = tempfile::tempdir().expect("temp history");
        let history = localpilot_store::PromptHistory::with_store(Some(
            temp.path().join("prompt-history.jsonl"),
        ));
        let mut app = app();
        app.begin_work();
        let cancel = CancellationToken::new();
        let mut queue = VecDeque::new();

        let _ = app.handle_input(
            InputAction::Insert("!echo SHELL_QUEUE_SECRET".to_string()),
            80,
        );
        assert!(!handle_turn_event(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            temp.path(),
            &image_capability(false),
        ));
        app.editor.insert("ordinary queued prompt");
        assert!(!handle_turn_event(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            temp.path(),
            &image_capability(false),
        ));

        assert_eq!(queue.len(), 2);
        let shell = queue.front().expect("queued shell").shell();
        let prompt = queue.back().expect("queued prompt").prompt();
        assert_eq!(shell.command.as_str(), "echo SHELL_QUEUE_SECRET");
        assert_eq!(prompt.text, "ordinary queued prompt");
        assert!(!format!("{queue:?}").contains("SHELL_QUEUE_SECRET"));
        let ordered_ids = app
            .timeline
            .items()
            .iter()
            .filter(|item| item.id == shell.item_id || item.id == prompt.item_id)
            .map(|item| item.id)
            .collect::<Vec<_>>();
        assert_eq!(ordered_ids, vec![shell.item_id, prompt.item_id]);
        assert_eq!(
            app.timeline.item(shell.item_id).expect("shell row").kind,
            ItemKind::Shell
        );
        assert!(
            app.timeline
                .item(prompt.item_id)
                .expect("prompt row")
                .pending
        );
        let stored = history.load();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].text, "ordinary queued prompt");
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn active_turn_never_queues_a_slash_command_as_provider_input() {
        let temp = tempfile::tempdir().expect("temp history");
        let history = localpilot_store::PromptHistory::with_store(Some(
            temp.path().join("prompt-history.jsonl"),
        ));
        let mut app = app();
        app.begin_work();
        let cancel = CancellationToken::new();
        let mut queue = VecDeque::new();

        let _ = app.handle_input(InputAction::Insert("/clear".to_string()), 80);
        assert!(!handle_turn_event(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            temp.path(),
            &image_capability(false),
        ));

        assert!(queue.is_empty());
        assert!(!cancel.is_cancelled());
        assert!(history.load().is_empty());
        assert!(app.timeline.items().iter().any(|item| {
            item.kind == ItemKind::Notice
                && item.text.contains("when the current operation is idle")
        }));
    }

    #[test]
    fn configured_providers_become_truthful_model_picker_values() {
        let mut config = localpilot_config::Config::default();
        config.providers.insert(
            "local".to_string(),
            localpilot_config::ProviderConfig {
                kind: "openai_compatible".to_string(),
                model: Some("fixture-model".to_string()),
                ..Default::default()
            },
        );
        config.providers.insert(
            "remote".to_string(),
            localpilot_config::ProviderConfig {
                kind: "anthropic".to_string(),
                ..Default::default()
            },
        );

        let values = fullscreen_model_values(&config, "local");
        assert_eq!(
            values
                .iter()
                .map(|value| value.name.as_str())
                .collect::<Vec<_>>(),
            vec!["local", "remote"]
        );
        assert!(values[0].description.contains("current"));
        assert!(values[0].description.contains("fixture-model"));
        assert!(!values[1].description.contains("current"));
        assert!(values[1].description.contains("provider default"));
    }

    #[test]
    fn shell_diagnostic_strips_only_the_registry_envelope() {
        assert_eq!(
            present_shell_diagnostic(
                "tool: run_shell\nstatus: error\noutput:\npermission denied for run_shell"
            ),
            "permission denied for run_shell"
        );
        assert_eq!(
            present_shell_diagnostic("cancelled by user"),
            "cancelled by user"
        );
    }

    #[test]
    fn buffered_approvals_are_all_denied_at_a_driver_boundary() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut replies = Vec::new();
        for number in 0..3 {
            let (reply, answer) = oneshot::channel();
            sender
                .send(ApprovalCall {
                    request: localpilot_tui::ApprovalRequest {
                        tool: format!("tool-{number}"),
                        target: "fixture".to_string(),
                        risk_class: "test".to_string(),
                    },
                    reply,
                })
                .expect("queue approval");
            replies.push(answer);
        }
        deny_buffered_approvals(&mut receiver);
        assert!(replies
            .iter_mut()
            .all(|answer| answer.try_recv() == Ok(false)));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[test]
    fn active_turn_paste_stays_compact_until_submit_then_queues_and_persists_raw() {
        let payload = (1..=12)
            .map(|line| format!("line {line} 界"))
            .collect::<Vec<_>>()
            .join("\n");
        let temp = tempfile::tempdir().expect("temp history");
        let history = localpilot_store::PromptHistory::with_store(Some(
            temp.path().join("prompt-history.jsonl"),
        ));
        let mut app = app();
        app.begin_work();
        let cancel = CancellationToken::new();
        let mut queue = VecDeque::new();

        assert!(!handle_turn_event(
            &mut app,
            Event::Paste(payload.clone()),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            temp.path(),
            &image_capability(false),
        ));
        assert_eq!(app.editor.text(), "[Paste #1 - 12 lines]");
        assert!(queue.is_empty());

        assert!(!handle_turn_event(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &cancel,
            &event_hit_map(),
            &mut queue,
            &history,
            temp.path(),
            &image_capability(false),
        ));
        assert_eq!(queue.front().expect("queued paste").prompt().text, payload);
        let entry = history.load().pop().expect("stored paste");
        assert_eq!(entry.text, "[Paste #1 - 12 lines]");
        assert_eq!(entry.pastes.len(), 1);
        assert_eq!(expand_history_entry(&entry), payload);
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn queued_image_prompts_keep_isolated_blocks_and_persist_no_image_bytes() {
        let temp = tempfile::tempdir().expect("temp history");
        let history = localpilot_store::PromptHistory::with_store(Some(
            temp.path().join("prompt-history.jsonl"),
        ));
        let mut app = app();
        app.begin_work();
        let cancel = CancellationToken::new();
        let mut queue = VecDeque::new();

        for (number, secret) in [(1, "IMAGE_SECRET_ONE"), (2, "IMAGE_SECRET_TWO")] {
            app.editor.insert(&format!("inspect {number} "));
            let placeholder = app
                .attach_image("image/png", secret, number * 1024)
                .expect("attach fixture image");
            assert!(!handle_turn_event(
                &mut app,
                Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
                &cancel,
                &event_hit_map(),
                &mut queue,
                &history,
                temp.path(),
                &image_capability(true),
            ));
            let queued = queue.back().expect("queued image prompt");
            let queued = queued.prompt();
            assert_eq!(queued.text, format!("inspect {number}"));
            assert_eq!(queued.attachments.len(), 1);
            let ContentBlock::Image { data, .. } = &queued.attachments[0] else {
                panic!("image content block");
            };
            assert_eq!(data, secret);
            let timeline_prompt = app.timeline.item(queued.item_id).expect("timeline prompt");
            assert!(timeline_prompt.text.contains(&placeholder));
        }

        assert_eq!(queue.len(), 2);
        assert!(!format!("{queue:?}").contains("IMAGE_SECRET"));
        assert_eq!(
            app.timeline
                .items()
                .iter()
                .filter(|item| item.text == "sending 1 image(s) with this prompt")
                .count(),
            2
        );
        let stored = std::fs::read_to_string(temp.path().join("prompt-history.jsonl"))
            .expect("stored history");
        assert!(!stored.contains("IMAGE_SECRET_ONE"));
        assert!(!stored.contains("IMAGE_SECRET_TWO"));
        let entries = history.load();
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|entry| entry.pastes.is_empty()));
        assert!(entries.iter().all(|entry| entry.text.contains("[image #")));
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn workspace_file_index_refreshes_an_open_mention_without_blocking_input() {
        let (sender, receiver) = std_mpsc::channel();
        let mut index = WorkspaceFileIndex {
            receiver,
            finished: false,
        };
        let mut app = app();
        let _ = app.handle_input(InputAction::Insert("@sam".to_string()), 80);
        assert!(app.has_input_overlay());

        sender
            .send(vec!["src/sample.rs".to_string()])
            .expect("workspace result");
        index.refresh(&mut app);
        assert!(index.finished);
        assert_eq!(app.handle_input(InputAction::Submit, 80), AppCommand::None);
        assert_eq!(app.editor.text(), "@src/sample.rs ");
    }

    #[test]
    fn first_frame_is_drawn_before_the_fullscreen_workspace_scan_starts() {
        let source = include_str!("fullscreen.rs");
        let first_frame = source
            .find("let _ = draw_synchronized(&mut terminal, &app)?;")
            .expect("first frame");
        let index_start = source
            .find("WorkspaceFileIndex::start(context.cwd.to_path_buf())")
            .expect("async workspace index start");
        assert!(first_frame < index_start);
    }

    #[test]
    fn active_turn_snapshots_image_capability_before_runtime_borrow() {
        let source = include_str!("fullscreen.rs");
        let snapshot = source
            .find("let image_capability = ImageCapabilitySnapshot")
            .expect("capability snapshot");
        let turn = source
            .find("let operation = runtime.run_turn_with_attachments")
            .expect("attachment turn");
        assert!(snapshot < turn);

        let mut app = app();
        attach_clipboard_image_with_capability(&mut app, &image_capability(false), false);
        assert!(app.timeline.items().iter().any(|item| item
            .text
            .contains("current model is not known to accept images")));
    }

    #[test]
    fn approval_denial_resolves_reply_and_clears_dialog() {
        let mut app = app();
        app.begin_work();
        app.request_approval("write_file", "fixture", "write");
        let (reply, mut answer) = oneshot::channel();
        let mut pending = Some(reply);
        let cancel = CancellationToken::new();

        assert!(!handle_approval_event(
            &mut app,
            Event::Key(press(KeyCode::Char('n'), KeyModifiers::NONE)),
            &mut pending,
            &cancel,
        ));
        assert_eq!(answer.try_recv(), Ok(false));
        assert!(app.dialog.is_none());
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn screen_reader_approval_has_no_enter_default_and_exposes_a_real_deny_key() {
        let mut app = app();
        app.capabilities.screen_reader = true;
        app.begin_work();
        app.request_approval("write_file", "fixture", "write");
        let (reply, mut answer) = oneshot::channel();
        let mut pending = Some(reply);
        let cancel = CancellationToken::new();

        assert!(!handle_approval_event(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &mut pending,
            &cancel,
        ));
        assert!(matches!(
            answer.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(app.dialog.is_some());

        assert!(!handle_approval_event(
            &mut app,
            Event::Key(press(KeyCode::Char('n'), KeyModifiers::NONE)),
            &mut pending,
            &cancel,
        ));
        assert_eq!(answer.try_recv(), Ok(false));
        assert!(app.dialog.is_none());
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn selected_text_keeps_first_ctrl_c_copy_precedence_during_approval() {
        let mut app = app();
        let item = app
            .timeline
            .push(localpilot_terminal_ui::ItemKind::Assistant, "copy me")
            .expect("timeline item");
        app.timeline.start_selection(ContentPoint {
            item_id: item,
            byte: 0,
        });
        app.timeline.extend_selection(ContentPoint {
            item_id: item,
            byte: 4,
        });
        app.begin_work();
        app.request_approval("write_file", "fixture", "write");
        let (reply, mut answer) = oneshot::channel();
        let mut pending = Some(reply);
        let cancel = CancellationToken::new();

        assert!(!handle_approval_event(
            &mut app,
            Event::Key(press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            &mut pending,
            &cancel,
        ));
        assert!(matches!(
            answer.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(app.dialog.is_some());
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn question_mouse_focus_and_enter_resolve_reply_without_cancelling_work() {
        let mut app = app();
        app.begin_work();
        app.request_question("Pick one", ["Red".to_string(), "Blue".to_string()]);
        let hit_map = draw_hit_map(&app, 120, 30);
        let (reply, mut answer) = oneshot::channel();
        let mut pending = Some(reply);
        let cancel = CancellationToken::new();

        let blue = hit_map.question_rows[1].area;
        assert!(!handle_question_event(
            &mut app,
            Event::Mouse(mouse(
                MouseEventKind::Down(MouseButton::Left),
                blue.x,
                blue.y,
            )),
            &mut pending,
            &cancel,
            &hit_map,
        ));
        assert!(!handle_question_event(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &mut pending,
            &cancel,
            &hit_map,
        ));
        assert_eq!(
            answer.try_recv(),
            Ok(ElicitationOutcome::Answered("Blue".to_string()))
        );
        assert!(app.dialog.is_none());
        assert!(!cancel.is_cancelled());
    }

    #[test]
    fn buffered_questions_are_cancelled_at_a_driver_boundary() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let mut answers = Vec::new();
        for _ in 0..3 {
            let (reply, answer) = oneshot::channel();
            sender
                .send(ElicitationCall {
                    request: localpilot_tools::ElicitationRequest {
                        question: "fixture".to_string(),
                        options: vec!["A".to_string(), "B".to_string()],
                    },
                    reply,
                })
                .expect("queue question");
            answers.push(answer);
        }

        cancel_buffered_elicitations(&mut receiver);
        for mut answer in answers {
            assert_eq!(answer.try_recv(), Ok(ElicitationOutcome::Cancelled));
        }
    }

    #[test]
    fn trust_dialog_preserves_double_ctrl_c_exit_contract() {
        let mut app = app();
        app.require_workspace_trust("fixture");
        let cwd = Path::new("fixture");
        let ingest = localpilot_config::IngestConfig::default();
        let ctrl_c = || Event::Key(press(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert!(!handle_trust_event(&mut app, ctrl_c(), cwd, &ingest));
        assert!(app.workspace_trust_pending());
        assert!(!handle_trust_event(
            &mut app,
            Event::Key(press(KeyCode::Char('x'), KeyModifiers::NONE)),
            cwd,
            &ingest,
        ));
        assert!(!handle_trust_event(&mut app, ctrl_c(), cwd, &ingest));
        assert!(handle_trust_event(&mut app, ctrl_c(), cwd, &ingest));
    }

    #[test]
    fn prompt_timestamp_is_local_hh_mm_shape() {
        let value = local_prompt_time();
        assert_eq!(value.len(), 5);
        assert_eq!(value.as_bytes()[2], b':');
        assert!(value[..2].parse::<u8>().is_ok_and(|hour| hour < 24));
        assert!(value[3..].parse::<u8>().is_ok_and(|minute| minute < 60));
    }

    #[test]
    fn prompt_timestamp_formats_the_precomputed_local_offset() {
        let offset = time::UtcOffset::from_hms(2, 30, 0).expect("offset");
        let local = time::OffsetDateTime::UNIX_EPOCH.to_offset(offset);
        assert_eq!(format_prompt_time(local), "02:30");
    }

    #[test]
    fn runtime_events_map_without_provider_or_view_state() {
        assert_eq!(
            map_runtime_event(RuntimeEvent::Usage(TokenUsage {
                input_tokens: 12,
                output_tokens: 34,
            })),
            RuntimeUpdate::Usage {
                input_tokens: 12,
                output_tokens: 34,
            }
        );
        assert_eq!(
            map_runtime_event(RuntimeEvent::Stopped(StopReason::Cancelled)),
            RuntimeUpdate::Stopped(StopState::Cancelled)
        );
    }

    #[test]
    fn terminal_mode_commands_enter_and_restore_in_safe_order() {
        let mut entered = Vec::new();
        write_required_modes(&mut entered, true).expect("required mode bytes");
        let text = String::from_utf8(entered).expect("ANSI is UTF-8");
        let alternate = text.find("?1049h").expect("alternate enter");
        let paste = text.find("?2004h").expect("paste enable");
        assert!(alternate < paste);

        // Crossterm uses the Windows console input API for mouse capture, so
        // there is deliberately no mouse escape sequence in this byte buffer.
        // On ANSI backends the sequence remains observable and ordered.
        #[cfg(not(windows))]
        {
            let mouse = text.find("?1000h").expect("mouse enable");
            assert!(alternate < mouse && mouse < paste);
        }

        let mut mouse_free = Vec::new();
        write_required_modes(&mut mouse_free, false).expect("mouse-free mode bytes");
        #[cfg(not(windows))]
        assert!(!String::from_utf8(mouse_free)
            .expect("ANSI is UTF-8")
            .contains("?1000h"));

        let mut restored = Vec::new();
        write_restore_modes(&mut restored, true, true).expect("restore mode bytes");
        let text = String::from_utf8(restored).expect("ANSI is UTF-8");
        let paste = text.find("?2004l").expect("paste disable");
        let alternate = text.find("?1049l").expect("alternate leave");
        assert!(paste < alternate);

        #[cfg(not(windows))]
        {
            let keyboard = text.find("<u").expect("keyboard pop");
            let mouse = text.find("?1000l").expect("mouse disable");
            assert!(keyboard < paste && paste < mouse && mouse < alternate);
        }

        let mut mouse_free_restore = Vec::new();
        write_restore_modes(&mut mouse_free_restore, false, false)
            .expect("mouse-free restore bytes");
        #[cfg(not(windows))]
        assert!(!String::from_utf8(mouse_free_restore)
            .expect("ANSI is UTF-8")
            .contains("?1000l"));
    }

    #[test]
    fn boolean_host_settings_accept_only_documented_values() {
        for value in ["true", "TRUE", "1"] {
            assert_eq!(parse_bool_setting(value), Some(true));
        }
        for value in ["false", "FALSE", "0"] {
            assert_eq!(parse_bool_setting(value), Some(false));
        }
        for value in ["", "yes", "2"] {
            assert_eq!(parse_bool_setting(value), None);
        }
    }

    #[test]
    fn unified_diff_parser_preserves_files_counts_kinds_and_line_numbers() {
        let files = parse_unified_diff(
            "diff --git a/src/one.rs b/src/one.rs\n\
             index 111..222 100644\n\
             --- a/src/one.rs\n\
             +++ b/src/one.rs\n\
             @@ -2,2 +2,2 @@\n\
             \x20keep\n\
             -old\n\
             +new\n\
             diff --git a/new.txt b/new.txt\n\
             new file mode 100644\n\
             --- /dev/null\n\
             +++ b/new.txt\n\
             @@ -0,0 +1 @@\n\
             +hello\n",
        );

        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "src/one.rs");
        assert_eq!((files[0].additions, files[0].deletions), (1, 1));
        assert_eq!(files[0].lines[1].old_line, Some(2));
        assert_eq!(files[0].lines[1].new_line, Some(2));
        assert_eq!(files[0].lines[2].kind, DiffLineKind::Deletion);
        assert_eq!(files[0].lines[2].old_line, Some(3));
        assert_eq!(files[0].lines[3].kind, DiffLineKind::Addition);
        assert_eq!(files[0].lines[3].new_line, Some(3));
        assert_eq!(files[1].status, "A");
        assert_eq!(files[1].path, "new.txt");
        assert_eq!(files[1].additions, 1);
    }

    #[test]
    fn unified_diff_parser_decodes_quoted_paths_without_running_diff_drivers() {
        let files = parse_unified_diff(
            "diff --git \"a/src/file name.rs\" \"b/src/file name.rs\"\n\
             --- \"a/src/file name.rs\"\n\
             +++ \"b/src/file name.rs\"\n\
             @@ -1 +1 @@\n\
             -old\n\
             +new\n\
             diff --git a/old.rs b/new.rs\n\
             similarity index 100%\n\
             rename from old.rs\n\
             rename to \"folder/new\\tname.rs\"\n",
        );

        assert_eq!(files[0].path, "src/file name.rs");
        assert_eq!(files[1].status, "R");
        assert_eq!(files[1].path, "folder/new\tname.rs");
        assert_eq!(
            split_git_path_fields("\"a/src/file name.rs\" \"b/src/file name.rs\""),
            ["a/src/file name.rs", "b/src/file name.rs"]
        );
    }

    #[test]
    fn runtime_event_replay_follows_bottom_or_preserves_a_held_content_anchor() {
        use std::fmt::Write as _;

        let mut seed = app();
        seed.begin_work();
        let mut seed_text = String::new();
        for number in 0..80 {
            writeln!(&mut seed_text, "seed {number:03}").expect("write fixture text");
        }
        seed.apply_runtime(RuntimeUpdate::Text(seed_text));
        seed.apply_runtime(RuntimeUpdate::Stopped(StopState::Done));

        let script = || {
            vec![
                RuntimeEvent::Text("stream 001\n".to_string()),
                RuntimeEvent::Text("stream 002\nSTREAM_TAIL".to_string()),
                RuntimeEvent::Usage(TokenUsage {
                    input_tokens: 12,
                    output_tokens: 34,
                }),
                RuntimeEvent::ToolStarted {
                    id: "fixture-tool".to_string(),
                    name: "inspect".to_string(),
                    detail: String::new(),
                },
                RuntimeEvent::ToolFinished {
                    id: "fixture-tool".to_string(),
                    name: "inspect".to_string(),
                    is_error: false,
                    cancelled: false,
                    output: "detail one\ndetail two".to_string(),
                    duration_ms: 25,
                },
                RuntimeEvent::Stopped(StopReason::Done),
            ]
        };

        let mut following = seed.clone();
        following.begin_work();
        for event in script() {
            following.apply_runtime(map_runtime_event(event));
        }
        let bottom = following.timeline.view(40, 8);
        assert!(bottom.rows.iter().any(|row| row.text == "STREAM_TAIL"));
        assert!(bottom
            .rows
            .iter()
            .any(|row| row.text == "inspect completed · 25 ms"));
        assert_eq!(following.usage, Some((12, 34)));
        assert_eq!(following.work, WorkState::Idle);

        let mut held = seed;
        held.timeline.scroll_by(-12, 40, 8);
        let ViewportAnchor::Held(anchor) = held.timeline.viewport else {
            panic!("seed must be held away from bottom");
        };
        held.begin_work();
        for event in script() {
            held.apply_runtime(map_runtime_event(event));
        }
        let held_view = held.timeline.view(31, 6);
        assert_eq!(
            held_view.rows.first().map(|row| row.item_id),
            Some(anchor.item_id)
        );
        assert_eq!(held.timeline.viewport, ViewportAnchor::Held(anchor));
    }
}
