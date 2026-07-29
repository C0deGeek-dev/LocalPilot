//! Crossterm host for the backend-neutral full-screen chat model.

use std::collections::VecDeque;
use std::io::{self, Stdout, Write};
use std::panic;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    self, BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use localpilot_harness::{ModelHealth, RuntimeEvent, SessionRuntime, StopReason};
use localpilot_terminal_ui::{
    render, AppCommand, AppModel, ColorSupport, Header, InputAction, ItemId, KeyboardSupport,
    PlanEntry, RecoveryState, RuntimeUpdate, StopState, TerminalCapabilities, Theme,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::key_input::{is_cancel, is_key_action};
use crate::repl::ApprovalCall;

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const CHAT_THEME_ENV: &str = "LOCALPILOT_CHAT_THEME";
static TERMINAL_MODES_ACTIVE: AtomicBool = AtomicBool::new(false);
static KEYBOARD_FLAGS_PUSHED: AtomicBool = AtomicBool::new(false);
static LOCAL_UTC_OFFSET: OnceLock<time::UtcOffset> = OnceLock::new();

pub(crate) fn capture_local_utc_offset() {
    let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let _ = LOCAL_UTC_OFFSET.set(offset);
}

pub(crate) struct HostContext<'a> {
    pub(crate) runtime: &'a mut SessionRuntime,
    pub(crate) approval_rx: &'a mut mpsc::UnboundedReceiver<ApprovalCall>,
    pub(crate) cwd: &'a Path,
    pub(crate) history: &'a localpilot_store::PromptHistory,
    pub(crate) ingest: &'a localpilot_config::IngestConfig,
    pub(crate) trust_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueuedPrompt {
    text: String,
    item_id: ItemId,
}

pub(crate) async fn run(
    header: Header,
    startup_events: impl IntoIterator<Item = RuntimeEvent>,
    context: HostContext<'_>,
) -> Result<()> {
    install_panic_restore_hook();
    let (mut modes, capabilities) = TerminalModes::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("initialize full-screen terminal")?;
    terminal.clear().context("clear full-screen terminal")?;
    let mut app = AppModel::new(header, capabilities);
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
    if !context.trust_required {
        crate::repl::start_session_knowledge_index(context.cwd, context.ingest);
    }
    let history_entries = context.history.load();
    app.seed_history(localpilot_store::project_texts(
        &history_entries,
        context.cwd,
    ));
    let result = run_event_loop(&mut terminal, &mut app, context).await;
    let _ = terminal.show_cursor();
    drop(terminal);
    modes.restore();
    result
}

fn apply_host_preferences(app: &mut AppModel) {
    let Some(value) = std::env::var_os(CHAT_THEME_ENV) else {
        return;
    };
    let Ok(value) = value.into_string() else {
        app.apply_runtime(RuntimeUpdate::Warning(format!(
            "{CHAT_THEME_ENV} contains non-Unicode text; using the default theme"
        )));
        return;
    };
    match value.parse::<Theme>() {
        Ok(theme) => app.theme = theme,
        Err(error) => app.apply_runtime(RuntimeUpdate::Warning(error.to_string())),
    }
}

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppModel,
    context: HostContext<'_>,
) -> Result<()> {
    let HostContext {
        runtime,
        approval_rx,
        cwd,
        history,
        ingest,
        trust_required: _,
    } = context;
    let mut queue = VecDeque::new();
    while !app.exit_requested {
        let hit_map = draw_synchronized(terminal, app)?;
        if !event::poll(EVENT_POLL_INTERVAL).context("poll full-screen terminal event")? {
            continue;
        }
        let next = event::read().context("read full-screen terminal event")?;
        if app.workspace_trust_pending() {
            if handle_trust_event(app, next, cwd, ingest) {
                break;
            }
            continue;
        }
        match next {
            Event::Key(key) if is_key_action(key) => {
                let Some(action) = map_key(key) else {
                    continue;
                };
                match app.handle_input(action, hit_map.editor_width) {
                    AppCommand::Exit => break,
                    AppCommand::Copy(text) => copy_to_clipboard(app, text),
                    AppCommand::Submit(prompt) => {
                        let Some(item_id) =
                            app.append_prompt(prompt.clone(), Some(local_prompt_time()), false)
                        else {
                            continue;
                        };
                        persist_prompt(app, history, cwd, &prompt);
                        if drive_prompt_chain(
                            terminal,
                            app,
                            runtime,
                            approval_rx,
                            QueuedPrompt {
                                text: prompt,
                                item_id,
                            },
                            &mut queue,
                            history,
                            cwd,
                            hit_map.editor_width,
                        )
                        .await?
                        {
                            break;
                        }
                    }
                    AppCommand::None | AppCommand::CancelWork => {}
                }
            }
            Event::Paste(text) => {
                let _ = app.handle_input(InputAction::Insert(text), hit_map.editor_width);
            }
            Event::FocusGained
            | Event::FocusLost
            | Event::Mouse(_)
            | Event::Resize(_, _)
            | Event::Key(_) => {}
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn drive_prompt_chain(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppModel,
    runtime: &mut SessionRuntime,
    approval_rx: &mut mpsc::UnboundedReceiver<ApprovalCall>,
    first: QueuedPrompt,
    queue: &mut VecDeque<QueuedPrompt>,
    history: &localpilot_store::PromptHistory,
    cwd: &Path,
    mut editor_width: u16,
) -> Result<bool> {
    let mut current = Some(first);
    while let Some(prompt) = current {
        let _ = app.activate_prompt(prompt.item_id);
        app.begin_work_before(queue.front().map(|queued| queued.item_id));
        if drive_turn(
            terminal,
            app,
            runtime,
            approval_rx,
            &prompt.text,
            editor_width,
            queue,
            history,
            cwd,
        )
        .await?
        {
            return Ok(true);
        }
        editor_width = draw_synchronized(terminal, app)?.editor_width;
        current = queue.pop_front();
    }
    Ok(false)
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
        KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
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
    prompt: &str,
    initial_editor_width: u16,
    queue: &mut VecDeque<QueuedPrompt>,
    history: &localpilot_store::PromptHistory,
    cwd: &Path,
) -> Result<bool> {
    let (events, mut rx) = broadcast::channel::<RuntimeEvent>(1024);
    let cancel = CancellationToken::new();
    let operation = runtime.run_turn(prompt, &events, &cancel);
    tokio::pin!(operation);
    let mut pending: Option<oneshot::Sender<bool>> = None;
    let mut tick = tokio::time::interval(EVENT_POLL_INTERVAL);
    let mut editor_width = initial_editor_width;
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            _ = tick.tick() => {
                for _ in 0..64 {
                    if !event::poll(Duration::ZERO).context("poll full-screen turn input")? {
                        break;
                    }
                    let next = event::read().context("read full-screen turn input")?;
                    let exit = if pending.is_some() {
                        handle_approval_event(app, next, &mut pending, &cancel)
                    } else {
                        handle_turn_event(
                            app,
                            next,
                            &cancel,
                            editor_width,
                            queue,
                            history,
                            cwd,
                        )
                    };
                    if exit {
                        cancel.cancel();
                        deny_pending(app, &mut pending);
                        return Ok(true);
                    }
                }
                editor_width = draw_synchronized(terminal, app)?.editor_width;
            }
            reason = &mut operation => {
                drain_runtime_events(app, &mut rx);
                app.apply_runtime(map_runtime_event(RuntimeEvent::Stopped(reason)));
                deny_pending(app, &mut pending);
                let _ = draw_synchronized(terminal, app)?;
                return Ok(false);
            }
            Some(call) = approval_rx.recv(), if pending.is_none() => {
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
        }
    }
}

fn handle_turn_event(
    app: &mut AppModel,
    event: Event,
    cancel: &CancellationToken,
    editor_width: u16,
    queue: &mut VecDeque<QueuedPrompt>,
    history: &localpilot_store::PromptHistory,
    cwd: &Path,
) -> bool {
    match event {
        Event::Key(key) if is_key_action(key) => {
            let Some(action) = map_key(key) else {
                return false;
            };
            match app.handle_input(action, editor_width) {
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
                AppCommand::Submit(prompt) => {
                    if let Some(item_id) =
                        app.append_prompt(prompt.clone(), Some(local_prompt_time()), true)
                    {
                        persist_prompt(app, history, cwd, &prompt);
                        queue.push_back(QueuedPrompt {
                            text: prompt,
                            item_id,
                        });
                    }
                    false
                }
                AppCommand::None => false,
            }
        }
        Event::Paste(text) => {
            let _ = app.handle_input(InputAction::Insert(text), editor_width);
            false
        }
        Event::FocusGained
        | Event::FocusLost
        | Event::Mouse(_)
        | Event::Resize(_, _)
        | Event::Key(_) => false,
    }
}

fn persist_prompt(
    app: &mut AppModel,
    history: &localpilot_store::PromptHistory,
    cwd: &Path,
    prompt: &str,
) {
    if let Err(error) = history.append(prompt, &[], cwd) {
        app.apply_runtime(RuntimeUpdate::Warning(format!(
            "prompt history could not be saved: {error}"
        )));
    }
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
        deny_pending(app, pending);
        cancel.cancel();
        return command == AppCommand::Exit;
    }
    let answer = match key.code {
        KeyCode::Char('y' | 'Y') | KeyCode::Enter => Some(true),
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

fn deny_pending(app: &mut AppModel, pending: &mut Option<oneshot::Sender<bool>>) {
    if let Some(reply) = pending.take() {
        let _ = reply.send(false);
    }
    app.clear_dialog();
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
        KeyCode::Char(character) if !ctrl && !alt => {
            Some(InputAction::Insert(character.to_string()))
        }
        KeyCode::Enter if alt || shift => Some(InputAction::Insert("\n".to_string())),
        KeyCode::Enter => Some(InputAction::Submit),
        KeyCode::Esc => Some(InputAction::InterruptWork),
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
        RuntimeEvent::ToolStarted { id, name } => RuntimeUpdate::ToolStarted { id, name },
        RuntimeEvent::ToolFinished {
            id,
            name,
            is_error,
            output,
        } => RuntimeUpdate::ToolFinished {
            id,
            name,
            is_error,
            output,
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
}

impl TerminalModes {
    fn enter() -> Result<(Self, TerminalCapabilities)> {
        terminal::enable_raw_mode().context("enable raw terminal mode")?;
        TERMINAL_MODES_ACTIVE.store(true, Ordering::Release);
        let mut guard = Self { active: true };
        let mut stdout = io::stdout();
        if let Err(error) = write_required_modes(&mut stdout) {
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
            mouse_capture: true,
            synchronized_output: true,
            keyboard: if enhanced {
                KeyboardSupport::Enhanced
            } else {
                KeyboardSupport::Basic
            },
            clipboard_write,
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

impl Drop for TerminalModes {
    fn drop(&mut self) {
        self.restore();
    }
}

fn write_required_modes(writer: &mut impl Write) -> io::Result<()> {
    execute!(
        writer,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        Hide
    )
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

fn write_restore_modes(writer: &mut impl Write, keyboard_flags_pushed: bool) -> io::Result<()> {
    if keyboard_flags_pushed {
        // Keyboard enhancement is opportunistic. Its legacy Windows command
        // may be unsupported, but that must never prevent required cleanup.
        let _ = execute!(writer, PopKeyboardEnhancementFlags);
    }
    execute!(
        writer,
        Show,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )
}

fn restore_terminal_modes() {
    if !TERMINAL_MODES_ACTIVE.swap(false, Ordering::AcqRel) {
        return;
    }
    let keyboard_flags_pushed = KEYBOARD_FLAGS_PUSHED.swap(false, Ordering::AcqRel);
    let _ = write_restore_modes(&mut io::stdout(), keyboard_flags_pushed);
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
    use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState};
    use localpilot_core::TokenUsage;
    use localpilot_terminal_ui::{ViewportAnchor, WorkState};

    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
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
            Some(InputAction::InterruptWork)
        );
        assert_eq!(
            map_key(press(KeyCode::Enter, KeyModifiers::SHIFT)),
            Some(InputAction::Insert("\n".to_string()))
        );
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
            80,
            &mut queue,
            &history,
            cwd,
        ));
        assert!(app.editor.text().is_empty());
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.front().expect("queued").text, "next prompt");
        assert!(
            app.timeline
                .item(queue.front().expect("queued").item_id)
                .expect("queued item")
                .pending
        );
        assert!(!cancel.is_cancelled());

        app.editor.insert("third prompt");
        assert!(!handle_turn_event(
            &mut app,
            Event::Key(press(KeyCode::Enter, KeyModifiers::NONE)),
            &cancel,
            80,
            &mut queue,
            &history,
            cwd,
        ));
        assert_eq!(
            queue
                .iter()
                .map(|queued| queued.text.as_str())
                .collect::<Vec<_>>(),
            vec!["next prompt", "third prompt"]
        );

        assert!(!handle_turn_event(
            &mut app,
            Event::Key(press(KeyCode::Esc, KeyModifiers::NONE)),
            &cancel,
            80,
            &mut queue,
            &history,
            cwd,
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
        write_required_modes(&mut entered).expect("required mode bytes");
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

        let mut restored = Vec::new();
        write_restore_modes(&mut restored, true).expect("restore mode bytes");
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
                },
                RuntimeEvent::ToolFinished {
                    id: "fixture-tool".to_string(),
                    name: "inspect".to_string(),
                    is_error: false,
                    output: "detail one\ndetail two".to_string(),
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
            .any(|row| row.text == "inspect completed"));
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
