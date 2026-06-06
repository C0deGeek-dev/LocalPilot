//! The event loop and input handling.
//!
//! The loop is driven by an iterator of [`AppInput`] so it runs deterministically
//! under a scripted source in tests; the real CLI feeds it crossterm events and a
//! mapped runtime-event stream.

use ratatui::backend::Backend;
use ratatui::Terminal;

use crate::render::render;
use crate::state::{AppState, Mode, Profile, UiEvent};

/// A terminal key press, decoupled from any specific terminal backend. The CLI's
/// terminal driver maps crossterm key events into these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Backspace,
    Delete,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    ScrollUp,
    ScrollDown,
    CtrlC,
}

/// One input to the loop: a mapped runtime event or a key press.
#[derive(Debug, Clone)]
pub enum AppInput {
    Ui(UiEvent),
    Key(Key),
}

/// A parsed slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashAction {
    SetMode(Mode),
    SetProfile(Profile),
    ToggleThinking,
    Clear,
    Compact,
    Search(Option<String>),
    Resume,
    WaitResume,
    Quit,
    Invalid { command: String, reason: String },
    Unknown(String),
}

/// Parse a slash command from an input line.
#[must_use]
pub fn parse_slash(line: &str) -> Option<SlashAction> {
    let command = line.trim().strip_prefix('/')?.trim();
    let (name, args) = command
        .split_once(char::is_whitespace)
        .map_or((command, ""), |(name, args)| (name, args.trim()));
    Some(match command {
        _ if name == "agent" && args.is_empty() => SlashAction::SetMode(Mode::Agent),
        _ if name == "harness" && args.is_empty() => SlashAction::SetMode(Mode::Harness),
        _ if name == "default" && args.is_empty() => SlashAction::SetProfile(Profile::Default),
        _ if name == "relaxed" && args.is_empty() => SlashAction::SetProfile(Profile::Relaxed),
        _ if name == "bypass" && args.is_empty() => SlashAction::SetProfile(Profile::Bypass),
        _ if matches!(name, "think" | "thinking") && args.is_empty() => SlashAction::ToggleThinking,
        _ if name == "clear" && args.is_empty() => SlashAction::Clear,
        _ if name == "compact" && args.is_empty() => SlashAction::Compact,
        _ if name == "search" => {
            let query = if args.is_empty() {
                None
            } else {
                Some(args.to_string())
            };
            SlashAction::Search(query)
        }
        _ if name == "resume" && args.is_empty() => SlashAction::Resume,
        _ if matches!(name, "wait-resume" | "wait_resume") && args.is_empty() => {
            SlashAction::WaitResume
        }
        _ if matches!(name, "quit" | "q") && args.is_empty() => SlashAction::Quit,
        _ if matches!(
            name,
            "agent"
                | "harness"
                | "default"
                | "relaxed"
                | "bypass"
                | "think"
                | "thinking"
                | "clear"
                | "compact"
                | "resume"
                | "wait-resume"
                | "wait_resume"
                | "quit"
                | "q"
        ) =>
        {
            SlashAction::Invalid {
                command: name.to_string(),
                reason: "this command does not take arguments".to_string(),
            }
        }
        _ => SlashAction::Unknown(command.to_string()),
    })
}

/// Apply one input to the state.
pub fn handle_input(state: &mut AppState, input: AppInput) {
    match input {
        AppInput::Ui(event) => state.apply(event),
        AppInput::Key(key) => handle_key(state, key),
    }
}

fn handle_key(state: &mut AppState, key: Key) {
    // The trust gate is the top-most modal: nothing else is reachable until the
    // folder is trusted or the session is declined.
    if state.trust.is_some() {
        match key {
            Key::Char('y' | 'Y') | Key::Enter => {
                state.trust = None;
                state.trusted = true;
            }
            Key::Char('n' | 'N') | Key::Esc | Key::CtrlC => {
                state.trust = None;
                state.should_quit = true;
            }
            _ => {}
        }
        return;
    }
    // Modal dialogs capture keys first.
    if state.approval.is_some() {
        if matches!(key, Key::Char('y') | Key::Char('n') | Key::Esc) {
            state.approval = None;
        }
        return;
    }
    if let Some(picker) = state.picker.as_mut() {
        match key {
            Key::Up => picker.selected = picker.selected.saturating_sub(1),
            Key::Down => {
                if picker.selected + 1 < picker.options.len() {
                    picker.selected += 1;
                }
            }
            Key::Enter | Key::Esc => state.picker = None,
            _ => {}
        }
        return;
    }

    match key {
        Key::Esc | Key::CtrlC => state.should_quit = true,
        Key::Enter => submit_input(state),
        Key::Backspace => state.backspace_input(),
        Key::Delete => state.delete_input(),
        Key::Left => state.move_input_left(),
        Key::Right => state.move_input_right(),
        Key::Up => state.move_input_up(),
        Key::Down => state.move_input_down(),
        Key::Home => state.move_input_home(),
        Key::End => state.move_input_end(),
        Key::PageUp => state.scroll_transcript_up(10),
        Key::PageDown => state.scroll_transcript_down(10),
        Key::ScrollUp => state.scroll_transcript_up(3),
        Key::ScrollDown => state.scroll_transcript_down(3),
        Key::Char(c) => state.insert_input(&c.to_string()),
    }
}

fn submit_input(state: &mut AppState) {
    let line = state.take_input_expanded();
    if line.trim().is_empty() {
        return;
    }
    if let Some(action) = parse_slash(&line) {
        apply_slash(state, action);
    } else {
        state.apply(UiEvent::UserMessage(line));
    }
}

fn apply_slash(state: &mut AppState, action: SlashAction) {
    match action {
        SlashAction::SetMode(mode) => state.mode = mode,
        SlashAction::SetProfile(profile) => state.profile = profile,
        SlashAction::ToggleThinking => state.thinking.visible = !state.thinking.visible,
        SlashAction::Clear => {
            state.clear_conversation_view();
            state.apply(UiEvent::Notice("conversation cleared".to_string()));
        }
        SlashAction::Compact => state.apply(UiEvent::Notice(
            "/compact is handled by the interactive host".to_string(),
        )),
        SlashAction::Search(query) => state.set_search(query),
        SlashAction::Resume => state.apply(UiEvent::Notice(
            "/resume is handled by the interactive host".to_string(),
        )),
        SlashAction::WaitResume => state.apply(UiEvent::Notice(
            "/wait-resume is handled by the interactive host".to_string(),
        )),
        SlashAction::Quit => state.should_quit = true,
        SlashAction::Invalid { command, reason } => {
            state.apply(UiEvent::Notice(format!("invalid /{command}: {reason}")));
        }
        SlashAction::Unknown(_) => {}
    }
}

/// Run the loop against a backend and a scripted input source, drawing after each
/// input until the state requests quit or the source is exhausted.
///
/// # Errors
/// Returns any drawing error from the terminal backend.
pub fn run<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &mut AppState,
    inputs: impl IntoIterator<Item = AppInput>,
) -> std::io::Result<()> {
    terminal.draw(|frame| render(frame, state))?;
    for input in inputs {
        handle_input(state, input);
        terminal.draw(|frame| render(frame, state))?;
        if state.should_quit {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Header, TrustPrompt};

    fn state() -> AppState {
        let mut state = AppState::new(
            Header {
                version: "0".into(),
                provider: "p".into(),
                model: "m".into(),
                workspace: "w".into(),
                session_id: "s".into(),
                update: None,
            },
            Mode::Agent,
            Profile::Default,
        );
        state.trust = Some(TrustPrompt {
            path: "/some/folder".into(),
        });
        state
    }

    #[test]
    fn trusting_the_folder_clears_the_gate_and_records_trust() {
        let mut state = state();
        handle_key(&mut state, Key::Char('y'));
        assert!(state.trust.is_none());
        assert!(state.trusted);
        assert!(!state.should_quit);
    }

    #[test]
    fn declining_the_trust_gate_quits() {
        let mut state = state();
        handle_key(&mut state, Key::Char('n'));
        assert!(state.trust.is_none());
        assert!(!state.trusted);
        assert!(state.should_quit);
    }

    #[test]
    fn the_trust_gate_swallows_unrelated_keys() {
        let mut state = state();
        handle_key(&mut state, Key::Char('x'));
        // Still gated; the stray key did not leak into the input.
        assert!(state.trust.is_some());
        assert!(state.input.is_empty());
    }

    #[test]
    fn navigation_keys_edit_the_middle_of_input() {
        let mut state = state();
        state.trust = None;
        for key in [
            Key::Char('a'),
            Key::Char('b'),
            Key::Char('d'),
            Key::Left,
            Key::Char('c'),
            Key::Home,
            Key::Delete,
            Key::End,
            Key::Backspace,
        ] {
            handle_key(&mut state, key);
        }
        assert_eq!(state.input, "bc");
        assert_eq!(state.input_cursor, state.input.len());
    }

    #[test]
    fn vertical_navigation_keys_move_between_input_rows() {
        let mut state = state();
        state.trust = None;
        state.input = "one\ntwo\nthree".to_string();
        state.input_cursor = "one\ntw".len();

        handle_key(&mut state, Key::Up);
        assert_eq!(&state.input[..state.input_cursor], "on");

        handle_key(&mut state, Key::Down);
        handle_key(&mut state, Key::Down);
        assert_eq!(&state.input[..state.input_cursor], "one\ntwo\nth");
    }

    #[test]
    fn busy_state_does_not_block_input_editing() {
        let mut state = state();
        state.trust = None;
        state.busy = true;
        state.input = "ac".to_string();
        state.input_cursor = 1;

        handle_key(&mut state, Key::Char('b'));
        handle_key(&mut state, Key::Left);
        handle_key(&mut state, Key::Right);

        assert_eq!(state.input, "abc");
        assert_eq!(state.input_cursor, 2);
    }

    #[test]
    fn transcript_scroll_keys_do_not_edit_input() {
        let mut state = state();
        state.trust = None;
        state.input = "prompt".to_string();
        state.input_cursor = state.input.len();
        state.streaming = (1..=20)
            .map(|line| format!("response line {line}"))
            .collect::<Vec<_>>()
            .join("\n");

        handle_key(&mut state, Key::PageUp);
        handle_key(&mut state, Key::ScrollUp);
        assert_eq!(state.input, "prompt");
        assert_eq!(state.transcript_scroll, 13);

        handle_key(&mut state, Key::ScrollDown);
        assert_eq!(state.transcript_scroll, 10);
    }

    #[test]
    fn transcript_scroll_up_is_capped_to_existing_output() {
        let mut state = state();
        state.trust = None;
        state.streaming = "one\ntwo\nthree".to_string();

        handle_key(&mut state, Key::PageUp);
        assert_eq!(state.transcript_scroll, 2);

        handle_key(&mut state, Key::ScrollDown);
        assert_eq!(state.transcript_scroll, 0);
    }
}
