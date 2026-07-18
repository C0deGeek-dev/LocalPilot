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
    Tab,
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
    CtrlC,
    /// Toggle prompt recall between this project's history and every project's.
    CtrlT,
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
    /// Compact the conversation. `force` trims even when it is within the
    /// configured budget (`/compact force` / `/compact_force`).
    Compact {
        force: bool,
    },
    /// Set the reasoning-effort level (validated by the host).
    SetEffort(String),
    /// Start a fresh session.
    NewSession,
    /// Branch the conversation into a new session (with a fork marker).
    Fork,
    /// Copy the conversation into a new session (no fork marker).
    CloneSession,
    /// Render the session's event tree.
    Tree,
    /// List this workspace's sessions.
    Sessions,
    /// Switch to (resume) the given session id.
    LoadSession(String),
    /// Continue the latest previous session, or a specific session when set.
    ContinueSession(Option<String>),
    /// Name (or rename) the current session (`/name` / `/rename`).
    NameSession(String),
    HarnessResume,
    WaitResume,
    /// Switch the active provider/model mid-session, or — with no provider — list
    /// the configured providers and their available models. `model` is only set
    /// when a model id follows the provider id.
    Model {
        provider: Option<String>,
        model: Option<String>,
    },
    Ingest(IngestAction),
    Knowledge(String),
    /// Research a topic. `Some(topic)` runs a one-shot research pass; `None`
    /// enters persistent research mode.
    Research(Option<String>),
    ContextBuild(String),
    /// Manage background processes started this session.
    Background(BackgroundCommand),
    Quit,
    Invalid {
        command: String,
        reason: String,
    },
    Unknown(String),
}

/// Parsed `/bg` subcommands for managing background processes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundCommand {
    /// List the running background processes.
    List,
    /// Stop a single process by id.
    Stop(String),
    /// Stop every background process.
    StopAll,
}

/// Parsed ingestion slash subcommands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestAction {
    Run,
    Preview,
    Status,
    Pause,
    Resume,
    Cancel,
    Refresh,
    Rebuild,
    Skipped,
    Include(String),
    Exclude(String),
    Forget(String),
    Review,
    Promote(String),
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
        _ if name == "unrestricted" && args.is_empty() => {
            SlashAction::SetProfile(Profile::Unrestricted)
        }
        _ if matches!(name, "think" | "thinking") && args.is_empty() => SlashAction::ToggleThinking,
        _ if name == "effort" && !args.is_empty() => SlashAction::SetEffort(args.to_string()),
        _ if name == "new" && args.is_empty() => SlashAction::NewSession,
        _ if name == "fork" && args.is_empty() => SlashAction::Fork,
        _ if name == "clone" && args.is_empty() => SlashAction::CloneSession,
        _ if name == "tree" && args.is_empty() => SlashAction::Tree,
        _ if name == "sessions" && args.is_empty() => SlashAction::Sessions,
        _ if name == "session" && !args.is_empty() => SlashAction::LoadSession(args.to_string()),
        _ if name == "session" => SlashAction::Invalid {
            command: "session".to_string(),
            reason: "usage: /session <id> (see /sessions)".to_string(),
        },
        _ if matches!(name, "name" | "rename") && !args.is_empty() => {
            SlashAction::NameSession(args.to_string())
        }
        _ if matches!(name, "name" | "rename") => SlashAction::Invalid {
            command: name.to_string(),
            reason: "usage: /name <text> — a name for this conversation".to_string(),
        },
        _ if name == "effort" => SlashAction::Invalid {
            command: "effort".to_string(),
            reason: "usage: /effort minimal|low|medium|high".to_string(),
        },
        _ if name == "clear" && args.is_empty() => SlashAction::Clear,
        _ if name == "compact" && args.is_empty() => SlashAction::Compact { force: false },
        _ if name == "compact" && args == "force" => SlashAction::Compact { force: true },
        _ if matches!(name, "compact_force" | "compact-force") && args.is_empty() => {
            SlashAction::Compact { force: true }
        }
        _ if name == "compact" => SlashAction::Invalid {
            command: "compact".to_string(),
            reason: "usage: /compact [force]".to_string(),
        },
        _ if matches!(name, "resume" | "continue") => {
            let id = if args.is_empty() {
                None
            } else {
                Some(args.to_string())
            };
            SlashAction::ContinueSession(id)
        }
        _ if name == "model" => {
            // `/model` lists; `/model <provider>` switches to that provider's
            // default model; `/model <provider> <model>` switches both.
            let (provider, model) = args
                .split_once(char::is_whitespace)
                .map_or((args, ""), |(provider, model)| (provider, model.trim()));
            SlashAction::Model {
                provider: (!provider.is_empty()).then(|| provider.to_string()),
                model: (!model.is_empty()).then(|| model.to_string()),
            }
        }
        _ if name == "harness-resume" && args.is_empty() => SlashAction::HarnessResume,
        _ if matches!(name, "wait-resume" | "wait_resume") && args.is_empty() => {
            SlashAction::WaitResume
        }
        _ if name == "ingest" => parse_ingest(args),
        _ if name == "knowledge" && !args.is_empty() => SlashAction::Knowledge(args.to_string()),
        _ if name == "knowledge" => SlashAction::Invalid {
            command: "knowledge".to_string(),
            reason: "usage: /knowledge <query>".to_string(),
        },
        _ if name == "research" && !args.is_empty() => {
            SlashAction::Research(Some(args.to_string()))
        }
        _ if name == "research" => SlashAction::Research(None),
        _ if name == "context" => parse_context(args),
        _ if name == "bg" => parse_bg(args),
        _ if matches!(name, "quit" | "q") && args.is_empty() => SlashAction::Quit,
        _ if matches!(
            name,
            "agent"
                | "harness"
                | "default"
                | "relaxed"
                | "bypass"
                | "unrestricted"
                | "think"
                | "thinking"
                | "clear"
                | "compact_force"
                | "compact-force"
                | "harness-resume"
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

fn parse_ingest(args: &str) -> SlashAction {
    if args.is_empty() {
        return SlashAction::Ingest(IngestAction::Run);
    }
    let (subcommand, rest) = args
        .split_once(char::is_whitespace)
        .map_or((args, ""), |(name, rest)| (name, rest.trim()));
    match subcommand {
        "preview" if rest.is_empty() => SlashAction::Ingest(IngestAction::Preview),
        "status" if rest.is_empty() => SlashAction::Ingest(IngestAction::Status),
        "pause" if rest.is_empty() => SlashAction::Ingest(IngestAction::Pause),
        "resume" if rest.is_empty() => SlashAction::Ingest(IngestAction::Resume),
        "cancel" if rest.is_empty() => SlashAction::Ingest(IngestAction::Cancel),
        "refresh" if rest.is_empty() => SlashAction::Ingest(IngestAction::Refresh),
        "rebuild" if rest.is_empty() => SlashAction::Ingest(IngestAction::Rebuild),
        "skipped" if rest.is_empty() => SlashAction::Ingest(IngestAction::Skipped),
        "include" if !rest.is_empty() => {
            SlashAction::Ingest(IngestAction::Include(rest.to_string()))
        }
        "exclude" if !rest.is_empty() => {
            SlashAction::Ingest(IngestAction::Exclude(rest.to_string()))
        }
        "forget" if !rest.is_empty() => {
            SlashAction::Ingest(IngestAction::Forget(rest.to_string()))
        }
        "review" if rest.is_empty() => SlashAction::Ingest(IngestAction::Review),
        "promote" if !rest.is_empty() => {
            SlashAction::Ingest(IngestAction::Promote(rest.to_string()))
        }
        _ => SlashAction::Invalid {
            command: "ingest".to_string(),
            reason: "usage: /ingest [preview|status|pause|resume|cancel|refresh|rebuild|skipped|include <path>|exclude <path>|forget <path-or-id>|review|promote <id>]".to_string(),
        },
    }
}

fn parse_bg(args: &str) -> SlashAction {
    if args.is_empty() {
        return SlashAction::Background(BackgroundCommand::List);
    }
    let (subcommand, rest) = args
        .split_once(char::is_whitespace)
        .map_or((args, ""), |(name, rest)| (name, rest.trim()));
    match subcommand {
        "list" if rest.is_empty() => SlashAction::Background(BackgroundCommand::List),
        "stop" if rest == "all" => SlashAction::Background(BackgroundCommand::StopAll),
        "stop" if !rest.is_empty() => {
            SlashAction::Background(BackgroundCommand::Stop(rest.to_string()))
        }
        _ => SlashAction::Invalid {
            command: "bg".to_string(),
            reason: "usage: /bg [list | stop <id> | stop all]".to_string(),
        },
    }
}

fn parse_context(args: &str) -> SlashAction {
    let (subcommand, rest) = args
        .split_once(char::is_whitespace)
        .map_or((args, ""), |(name, rest)| (name, rest.trim()));
    if subcommand == "build" && !rest.is_empty() {
        SlashAction::ContextBuild(rest.to_string())
    } else {
        SlashAction::Invalid {
            command: "context".to_string(),
            reason: "usage: /context build <task>".to_string(),
        }
    }
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

    // While the slash-command autocomplete is open it captures input: arrows
    // navigate, Enter/Tab accept the highlighted command, Esc dismisses, and
    // edits refilter the list (closing it once the input leaves slash context).
    if state.slash_picker.is_some() {
        match key {
            // Ctrl+C is handled globally (staged clear-then-quit): a slash
            // command in progress is non-empty input, so the first press clears
            // it (closing this overlay); a second press on empty input quits.
            Key::CtrlC => state.ctrl_c(),
            Key::Up => state.slash_picker_prev(),
            Key::Down => state.slash_picker_next(),
            Key::Enter | Key::Tab => state.slash_picker_select(),
            Key::Esc => state.close_slash_picker(),
            Key::Backspace => {
                state.backspace_input();
                state.refresh_or_close_slash_picker();
            }
            Key::Char(c) => {
                state.insert_input(&c.to_string());
                state.refresh_or_close_slash_picker();
            }
            _ => {}
        }
        return;
    }

    // While the `@` file-mention autocomplete is open it captures input the same
    // way: arrows navigate, Enter/Tab insert the highlighted path, Esc dismisses,
    // and edits refilter (closing it once the input leaves mention context).
    if state.file_picker.is_some() {
        match key {
            // Ctrl+C is handled globally (staged clear-then-quit): same rule as
            // the slash picker above — the first press clears the in-progress
            // mention (closing this overlay), a second on empty input quits.
            Key::CtrlC => state.ctrl_c(),
            Key::Up => state.file_picker_prev(),
            Key::Down => state.file_picker_next(),
            Key::Enter | Key::Tab => state.file_picker_select(),
            Key::Esc => state.close_file_picker(),
            Key::Backspace => {
                state.backspace_input();
                state.refresh_or_close_file_picker();
            }
            Key::Char(c) => {
                state.insert_input(&c.to_string());
                state.refresh_or_close_file_picker();
            }
            _ => {}
        }
        return;
    }

    match key {
        Key::Esc => state.should_quit = true,
        Key::CtrlC => state.ctrl_c(),
        Key::Enter => submit_input(state),
        Key::Backspace => state.backspace_input(),
        Key::Delete => state.delete_input(),
        Key::Left => state.move_input_left(),
        Key::Right => state.move_input_right(),
        Key::Up => {
            if state.input_cursor_is_on_first_line() {
                let _ = state.recall_previous_input();
            } else {
                state.move_input_up();
            }
        }
        Key::Down => {
            if state.input_cursor_is_on_last_line() {
                let _ = state.recall_next_input();
            } else {
                state.move_input_down();
            }
        }
        Key::Home => state.move_input_home(),
        Key::End => state.move_input_end(),
        // The terminal's own scrollback handles scrolling now, so the page keys
        // no longer drive an in-app transcript scroll.
        Key::PageUp | Key::PageDown => {}
        Key::Tab => {}
        Key::CtrlT => {
            let all = state.toggle_history_scope();
            let scope = if all { "all projects" } else { "this project" };
            state.apply(UiEvent::Notice(format!("prompt history: {scope}")));
        }
        Key::Char(c) => {
            state.insert_input(&c.to_string());
            // A '/' typed at the start of the line opens the slash-command
            // autocomplete; an '@' (at the start or after whitespace) opens the
            // file-mention autocomplete. Once open, further edits are handled
            // above.
            if c == '/' && state.is_in_slash_context() {
                state.open_slash_picker(state.input[..state.input_cursor].to_string());
            } else if c == '@' && state.is_in_mention_context() {
                state.open_file_picker();
            }
        }
    }
}

fn submit_input(state: &mut AppState) {
    let submitted = state.take_input_for_submit();
    let (shown, expanded) = (submitted.shown, submitted.prompt);
    if expanded.trim().is_empty() {
        return;
    }
    if let Some(action) = parse_slash(&expanded) {
        apply_slash(state, action);
    } else {
        state.apply(UiEvent::UserMessage(shown));
    }
}

fn apply_slash(state: &mut AppState, action: SlashAction) {
    match action {
        SlashAction::SetMode(mode) => state.mode = mode,
        SlashAction::SetProfile(profile) => state.profile = profile,
        SlashAction::ToggleThinking => state.thinking.visible = !state.thinking.visible,
        SlashAction::SetEffort(level) => {
            state.apply(UiEvent::Notice(format!("reasoning effort: {level}")));
        }
        SlashAction::NewSession
        | SlashAction::Fork
        | SlashAction::CloneSession
        | SlashAction::Tree
        | SlashAction::Sessions
        | SlashAction::LoadSession(_)
        | SlashAction::ContinueSession(_)
        | SlashAction::NameSession(_) => {
            state.apply(UiEvent::Notice(
                "session lifecycle commands are handled by the host".to_string(),
            ));
        }
        SlashAction::Clear => {
            state.clear_conversation_view();
            state.apply(UiEvent::Notice("conversation cleared".to_string()));
        }
        SlashAction::Compact { .. } => state.apply(UiEvent::Notice(
            "/compact is handled by the interactive host".to_string(),
        )),
        SlashAction::HarnessResume => state.apply(UiEvent::Notice(
            "/harness-resume is handled by the interactive host".to_string(),
        )),
        SlashAction::WaitResume => state.apply(UiEvent::Notice(
            "/wait-resume is handled by the interactive host".to_string(),
        )),
        SlashAction::Ingest(_) => state.apply(UiEvent::Notice(
            "/ingest is handled by the interactive host".to_string(),
        )),
        SlashAction::Knowledge(_) => state.apply(UiEvent::Notice(
            "/knowledge is handled by the interactive host".to_string(),
        )),
        SlashAction::ContextBuild(_) => state.apply(UiEvent::Notice(
            "/context is handled by the interactive host".to_string(),
        )),
        SlashAction::Research(_) => state.apply(UiEvent::Notice(
            "/research is handled by the interactive host".to_string(),
        )),
        SlashAction::Background(_) => state.apply(UiEvent::Notice(
            "/bg is handled by the interactive host".to_string(),
        )),
        SlashAction::Model { .. } => state.apply(UiEvent::Notice(
            "/model is handled by the interactive host".to_string(),
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
                session_name: None,
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
    fn ctrl_c_while_a_slash_command_is_typed_clears_first_then_quits() {
        let mut state = state();
        state.trust = None;
        state.input = "/com".to_string();
        state.input_cursor = state.input.len();
        state.open_slash_picker("/com".to_string());
        assert!(state.slash_picker.is_some());

        // First press abandons the in-progress command and dismisses the picker,
        // without quitting — matching the shell convention Ctrl+C users expect.
        handle_key(&mut state, Key::CtrlC);
        assert!(!state.should_quit, "first Ctrl+C must clear, not quit");
        assert!(state.input.is_empty());
        assert!(state.slash_picker.is_none());

        // Second press, now on an empty composer, quits.
        handle_key(&mut state, Key::CtrlC);
        assert!(state.should_quit, "second Ctrl+C on empty input must quit");
    }

    #[test]
    fn ctrl_c_clears_a_typed_prompt_before_quitting() {
        let mut state = state();
        state.trust = None;
        state.input = "half a prompt".to_string();
        state.input_cursor = state.input.len();

        handle_key(&mut state, Key::CtrlC);
        assert!(!state.should_quit, "first Ctrl+C must clear, not quit");
        assert!(state.input.is_empty());
        assert_eq!(state.input_cursor, 0);

        handle_key(&mut state, Key::CtrlC);
        assert!(state.should_quit, "second Ctrl+C on empty input must quit");
    }

    #[test]
    fn ctrl_c_on_an_empty_composer_quits_immediately() {
        let mut state = state();
        state.trust = None;
        assert!(state.input.is_empty());

        handle_key(&mut state, Key::CtrlC);
        assert!(state.should_quit, "Ctrl+C on empty input quits right away");
    }

    #[test]
    fn typing_c_into_a_slash_command_does_not_quit() {
        let mut state = state();
        state.trust = None;
        state.input = "/".to_string();
        state.input_cursor = state.input.len();
        state.open_slash_picker("/".to_string());

        handle_key(&mut state, Key::Char('c'));
        assert!(!state.should_quit, "plain 'c' must not quit");
        assert_eq!(state.input, "/c");
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
    fn up_and_down_recall_previous_inputs_shell_style() {
        let mut state = state();
        state.trust = None;

        state.insert_input("first prompt");
        handle_key(&mut state, Key::Enter);
        state.insert_input("second prompt");
        handle_key(&mut state, Key::Enter);

        state.insert_input("draft");
        handle_key(&mut state, Key::Up);
        assert_eq!(state.input, "second prompt");
        assert_eq!(state.input_cursor, state.input.len());

        handle_key(&mut state, Key::Up);
        assert_eq!(state.input, "first prompt");

        handle_key(&mut state, Key::Down);
        assert_eq!(state.input, "second prompt");

        handle_key(&mut state, Key::Down);
        assert_eq!(state.input, "draft");
    }

    #[test]
    fn ctrl_t_toggles_recall_scope_and_posts_a_notice() {
        let mut state = state();
        state.trust = None;
        state.seed_input_history(
            vec![crate::state::RecallEntry::text_only("project-only")],
            vec![
                crate::state::RecallEntry::text_only("project-only"),
                crate::state::RecallEntry::text_only("another-project"),
            ],
        );

        // Default scope: recall sees only this project's prompt.
        handle_key(&mut state, Key::Up);
        assert_eq!(state.input, "project-only");

        // Ctrl-T switches to all projects, reachable by recall, with a notice.
        state.input.clear();
        state.input_cursor = 0;
        handle_key(&mut state, Key::CtrlT);
        assert!(matches!(
            state.transcript.last(),
            Some(line) if line.speaker == "system" && line.text.contains("all projects")
        ));
        handle_key(&mut state, Key::Up);
        assert_eq!(state.input, "another-project");
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
    fn the_page_keys_no_longer_edit_the_input() {
        let mut state = state();
        state.trust = None;
        state.input = "prompt".to_string();
        state.input_cursor = state.input.len();

        // Scrolling is the terminal's job now; the page keys are inert and must
        // not leak into the composer.
        handle_key(&mut state, Key::PageUp);
        handle_key(&mut state, Key::PageDown);
        assert_eq!(state.input, "prompt");
        assert_eq!(state.input_cursor, state.input.len());
    }

    #[test]
    fn name_and_rename_slash_commands_are_parsed() {
        assert_eq!(
            parse_slash("/name my refactor"),
            Some(SlashAction::NameSession("my refactor".to_string()))
        );
        // `/rename` is an alias for `/name`.
        assert_eq!(
            parse_slash("/rename my refactor"),
            Some(SlashAction::NameSession("my refactor".to_string()))
        );
        // A bare `/name` with no text is a usage error, not a session op.
        assert!(matches!(
            parse_slash("/name"),
            Some(SlashAction::Invalid { command, .. }) if command == "name"
        ));
    }

    #[test]
    fn ingest_slash_commands_are_parsed() {
        assert_eq!(
            parse_slash("/ingest"),
            Some(SlashAction::Ingest(IngestAction::Run))
        );
        assert_eq!(
            parse_slash("/ingest preview"),
            Some(SlashAction::Ingest(IngestAction::Preview))
        );
        assert_eq!(
            parse_slash("/ingest include src/lib.rs"),
            Some(SlashAction::Ingest(IngestAction::Include(
                "src/lib.rs".to_string()
            )))
        );
        assert_eq!(
            parse_slash("/ingest promote item-1"),
            Some(SlashAction::Ingest(IngestAction::Promote(
                "item-1".to_string()
            )))
        );
    }

    #[test]
    fn bg_slash_commands_are_parsed() {
        assert_eq!(
            parse_slash("/bg"),
            Some(SlashAction::Background(BackgroundCommand::List))
        );
        assert_eq!(
            parse_slash("/bg list"),
            Some(SlashAction::Background(BackgroundCommand::List))
        );
        assert_eq!(
            parse_slash("/bg stop bg-1"),
            Some(SlashAction::Background(BackgroundCommand::Stop(
                "bg-1".to_string()
            )))
        );
        assert_eq!(
            parse_slash("/bg stop all"),
            Some(SlashAction::Background(BackgroundCommand::StopAll))
        );
        assert!(matches!(
            parse_slash("/bg frobnicate"),
            Some(SlashAction::Invalid { command, .. }) if command == "bg"
        ));
    }

    #[test]
    fn model_slash_command_parses_each_form() {
        // No args lists configured providers and their models.
        assert_eq!(
            parse_slash("/model"),
            Some(SlashAction::Model {
                provider: None,
                model: None
            })
        );
        // One token switches to that provider's default model.
        assert_eq!(
            parse_slash("/model anthropic"),
            Some(SlashAction::Model {
                provider: Some("anthropic".to_string()),
                model: None
            })
        );
        // A trailing model id switches both, and surrounding whitespace is trimmed.
        assert_eq!(
            parse_slash("/model  anthropic   claude-x "),
            Some(SlashAction::Model {
                provider: Some("anthropic".to_string()),
                model: Some("claude-x".to_string())
            })
        );
    }

    #[test]
    fn knowledge_and_context_slash_commands_are_parsed() {
        assert_eq!(
            parse_slash("/knowledge parser"),
            Some(SlashAction::Knowledge("parser".to_string()))
        );
        assert_eq!(
            parse_slash("/context build fix parser"),
            Some(SlashAction::ContextBuild("fix parser".to_string()))
        );
    }
}
