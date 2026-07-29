//! Crossterm host for the backend-neutral full-screen chat model.

use std::io::{self, Stdout, Write};
use std::panic;
use std::sync::atomic::{AtomicBool, Ordering};
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
use localpilot_harness::{ModelHealth, RuntimeEvent, StopReason};
use localpilot_terminal_ui::{
    render, AppCommand, AppModel, ColorSupport, Header, InputAction, KeyboardSupport, PlanEntry,
    RecoveryState, RuntimeUpdate, StopState, TerminalCapabilities, Theme,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::key_input::{is_cancel, is_key_action};

const EVENT_POLL_INTERVAL: Duration = Duration::from_millis(50);
const CHAT_THEME_ENV: &str = "LOCALPILOT_CHAT_THEME";
static TERMINAL_MODES_ACTIVE: AtomicBool = AtomicBool::new(false);
static KEYBOARD_FLAGS_PUSHED: AtomicBool = AtomicBool::new(false);

pub(crate) fn run(
    header: Header,
    startup_events: impl IntoIterator<Item = RuntimeEvent>,
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
    let result = run_idle_loop(&mut terminal, &mut app);
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

fn run_idle_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppModel,
) -> Result<()> {
    while !app.exit_requested {
        let hit_map = draw_synchronized(terminal, app)?;
        if !event::poll(EVENT_POLL_INTERVAL).context("poll full-screen terminal event")? {
            continue;
        }
        match event::read().context("read full-screen terminal event")? {
            Event::Key(key) if is_key_action(key) => {
                let Some(action) = map_key(key) else {
                    continue;
                };
                match app.handle_input(action, hit_map.editor_width) {
                    AppCommand::Exit => break,
                    AppCommand::Copy(text) => copy_to_clipboard(app, text),
                    AppCommand::None | AppCommand::CancelWork | AppCommand::Submit(_) => {}
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

    #[test]
    fn ctrl_c_maps_to_contextual_interrupt_handling() {
        assert_eq!(
            map_key(press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(InputAction::CancelOrExit)
        );
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

        let header = Header {
            version: "0".to_string(),
            provider: "fixture".to_string(),
            model: "fixture-model".to_string(),
            workspace: "fixture-workspace".to_string(),
            session_id: "fixture-session".to_string(),
            session_name: None,
        };
        let mut seed = AppModel::new(header, TerminalCapabilities::default());
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
