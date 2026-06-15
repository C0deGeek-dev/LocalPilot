//! Inline TUI render snapshots and behaviour.
//!
//! Finished transcript items flow into native scrollback via the host; these
//! tests cover the two surfaces the crate owns: the per-item [`history_block_text`]
//! and [`header_text`] blocks the host inserts above the viewport, and the live
//! region that [`render`] draws (activity tail, composer, status).
#![allow(clippy::unwrap_used)]

use localpilot_tui::{
    handle_input, header_text, history_block_text, parse_slash, run, ActiveTool, AppInput,
    AppState, ApprovalRequest, Header, Key, Mode, Picker, Profile, SlashAction, TranscriptLine,
    TrustPrompt, UiEvent,
};
use ratatui::backend::{Backend, TestBackend};
use ratatui::buffer::Buffer;
use ratatui::text::Text;
use ratatui::Terminal;

fn header() -> Header {
    Header {
        version: "0.1.0".to_string(),
        provider: "local".to_string(),
        model: "test-model".to_string(),
        workspace: "demo".to_string(),
        session_id: "ab12cd".to_string(),
        update: None,
    }
}

fn base() -> AppState {
    let mut state = AppState::new(header(), Mode::Agent, Profile::Default);
    state.footer.tokens_in = 120;
    state.footer.tokens_out = 48;
    state.footer.tokens_per_sec = 24.0;
    state.footer.context_used = 1200;
    state.footer.context_limit = 8000;
    state
}

fn buffer_string(buffer: &Buffer) -> String {
    let area = buffer.area;
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            if let Some(cell) = buffer.cell((x, y)) {
                out.push_str(cell.symbol());
            }
        }
        out.push('\n');
    }
    out
}

fn render_string(state: &AppState, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| localpilot_tui::render(frame, state))
        .unwrap();
    buffer_string(terminal.backend().buffer())
}

/// Flatten a styled [`Text`] into plain newline-joined strings.
fn text_to_string(text: &Text) -> String {
    text.lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// --- Live region snapshots ----------------------------------------------------

#[test]
fn live_region_snapshot() {
    let mut state = base();
    state.input = "what changed?".to_string();
    insta::assert_snapshot!(render_string(&state, 90, 8));
}

#[test]
fn streaming_live_region_snapshot() {
    let mut state = base();
    state.streaming = "Streaming the answer live...".to_string();
    insta::assert_snapshot!(render_string(&state, 90, 8));
}

#[test]
fn approval_modal_snapshot() {
    let mut state = base();
    state.approval = Some(ApprovalRequest {
        tool: "run_shell".to_string(),
        target: "rm -rf build".to_string(),
        risk_class: "destructive".to_string(),
    });
    insta::assert_snapshot!(render_string(&state, 90, 12));
}

// --- History / header blocks (emitted into native scrollback) -----------------

#[test]
fn history_block_text_prefixes_speakers_and_indents_continuations() {
    let you = history_block_text(&TranscriptLine {
        speaker: "you".to_string(),
        text: "summarize the parser".to_string(),
    });
    assert_eq!(text_to_string(&you), "you: summarize the parser");

    let assistant = history_block_text(&TranscriptLine {
        speaker: "assistant".to_string(),
        text: "line one\nline two".to_string(),
    });
    assert_eq!(
        text_to_string(&assistant),
        "assistant: line one\n  line two"
    );

    let tool = history_block_text(&TranscriptLine {
        speaker: "tool".to_string(),
        text: "read_file ok: hello".to_string(),
    });
    assert_eq!(text_to_string(&tool), "[tool] read_file ok: hello");
}

#[test]
fn header_text_carries_session_identity() {
    let rendered = text_to_string(&header_text(&header()));
    assert!(rendered.contains("LocalPilot 0.1.0"));
    assert!(rendered.contains("local/test-model"));
    assert!(rendered.contains("ws:demo"));
    assert!(rendered.contains("session:ab12cd"));
}

// --- Scrollback draining ------------------------------------------------------

#[test]
fn finished_items_drain_to_scrollback_once() {
    let mut state = base();
    state.apply(UiEvent::UserMessage("hello".to_string()));
    let first = state.drain_for_scrollback();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].speaker, "you");
    // Nothing new is finished, so a second drain is empty.
    assert!(state.drain_for_scrollback().is_empty());

    state.apply(UiEvent::TextDelta("answer".to_string()));
    state.apply(UiEvent::TurnComplete);
    let second = state.drain_for_scrollback();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].speaker, "assistant");
    assert_eq!(second[0].text, "answer");
}

#[test]
fn streaming_shows_live_then_settles_into_scrollback_once() {
    let mut state = base();
    state.apply(UiEvent::TextDelta("partial answer".to_string()));
    // While streaming, the live region shows it and nothing is drained yet.
    assert!(render_string(&state, 90, 8).contains("assistant: partial answer"));
    assert!(state.drain_for_scrollback().is_empty());
    // On completion it becomes exactly one finished block bound for scrollback.
    state.apply(UiEvent::TurnComplete);
    let drained = state.drain_for_scrollback();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].text, "partial answer");
    assert!(state.streaming.is_empty());
}

#[test]
fn a_running_tool_is_live_then_its_result_lands_in_scrollback() {
    let mut state = base();
    state.apply(UiEvent::ToolStarted {
        id: "call_1".to_string(),
        name: "run_shell".to_string(),
    });
    assert_eq!(
        state.active_tools,
        vec![ActiveTool {
            id: "call_1".to_string(),
            name: "run_shell".to_string()
        }]
    );
    assert!(
        state.drain_for_scrollback().is_empty(),
        "a running tool is not committed to scrollback"
    );

    state.apply(UiEvent::ToolFinished {
        id: "call_1".to_string(),
        name: "run_shell".to_string(),
        is_error: false,
        output: "tool: run_shell\nstatus: ok\noutput:\ndone".to_string(),
    });
    assert!(state.active_tools.is_empty());
    let drained = state.drain_for_scrollback();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].speaker, "tool");
    assert_eq!(drained[0].text, "run_shell ok: done");
}

// --- Live region content ------------------------------------------------------

#[test]
fn bypass_profile_is_visible_in_the_footer() {
    let mut state = base();
    state.profile = Profile::Bypass;
    assert!(render_string(&state, 90, 8).contains("profile:BYPASS"));
}

#[test]
fn trust_modal_shows_the_full_workspace_path() {
    let mut state = base();
    state.trust = Some(TrustPrompt {
        path: r"D:\repos\rust\localpilot".to_string(),
    });
    let rendered = render_string(&state, 90, 16);
    assert!(rendered.contains(r"D:\repos\rust\localpilot"));
    assert!(rendered.contains("trust this folder?"));
}

#[test]
fn input_cursor_is_visible_at_the_edit_position() {
    let mut state = base();
    state.input = "abcd".to_string();
    state.input_cursor = 2;
    let mut terminal = Terminal::new(TestBackend::new(90, 18)).unwrap();
    terminal
        .draw(|frame| localpilot_tui::render(frame, &state))
        .unwrap();

    // The activity tail fills the top; the one-line input box sits above the
    // two-row status, so its content row is y=14 and the cursor is at column 3.
    assert_eq!(
        terminal.backend_mut().get_cursor_position().unwrap(),
        ratatui::layout::Position::new(3, 14)
    );
}

// --- Event loop / input -------------------------------------------------------

#[test]
fn app_starts_and_quits_cleanly_under_a_scripted_source() {
    let mut state = base();
    let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
    run(&mut terminal, &mut state, vec![AppInput::Ui(UiEvent::Quit)]).unwrap();
    assert!(state.should_quit);
}

#[test]
fn a_slash_command_triggers_the_matching_action() {
    let mut state = base();
    assert!(!state.thinking.visible);
    state.input = "/think".to_string();
    handle_input(&mut state, AppInput::Key(Key::Enter));
    assert!(state.thinking.visible, "/think should toggle the panel");
    assert!(state.input.is_empty(), "input is cleared after a command");
}

#[test]
fn resume_slash_commands_are_parsed_for_the_host() {
    assert_eq!(
        parse_slash("/resume"),
        Some(SlashAction::ContinueSession(None))
    );
    assert_eq!(
        parse_slash("/continue session-1"),
        Some(SlashAction::ContinueSession(Some("session-1".to_string())))
    );
    assert_eq!(
        parse_slash("/harness-resume"),
        Some(SlashAction::HarnessResume)
    );
    assert_eq!(parse_slash("/wait-resume"), Some(SlashAction::WaitResume));
}

#[test]
fn clear_compact_and_search_slash_commands_are_parsed() {
    assert_eq!(parse_slash("/clear"), Some(SlashAction::Clear));
    assert_eq!(
        parse_slash("/compact force"),
        Some(SlashAction::Compact { force: true })
    );
    assert_eq!(
        parse_slash("/search parser errors"),
        Some(SlashAction::Search(Some("parser errors".to_string())))
    );
    assert_eq!(parse_slash("/search"), Some(SlashAction::Search(None)));
    assert_eq!(parse_slash("/q"), Some(SlashAction::Quit));
    assert!(matches!(
        parse_slash("/clear now"),
        Some(SlashAction::Invalid { command, .. }) if command == "clear"
    ));
    assert_eq!(
        parse_slash("/not-a-command"),
        Some(SlashAction::Unknown("not-a-command".to_string()))
    );
}

#[test]
fn search_command_sets_and_clears_search_state() {
    let mut state = base();
    state.input = "/search parser".to_string();
    state.input_cursor = state.input.len();
    handle_input(&mut state, AppInput::Key(Key::Enter));
    assert_eq!(state.search, Some("parser".to_string()));

    state.input = "/search".to_string();
    state.input_cursor = state.input.len();
    handle_input(&mut state, AppInput::Key(Key::Enter));
    assert!(state.search.is_none());
}

#[test]
fn clear_command_resets_conversation_view_but_keeps_session_settings() {
    let mut state = base();
    state.mode = Mode::Harness;
    state.profile = Profile::Bypass;
    state.trusted = true;
    state.streaming = "partial".to_string();
    state.thinking.text = "reasoning".to_string();
    let session_id = state.header.session_id.clone();

    state.input = "/clear".to_string();
    state.input_cursor = state.input.len();
    handle_input(&mut state, AppInput::Key(Key::Enter));

    assert_eq!(state.mode, Mode::Harness);
    assert_eq!(state.profile, Profile::Bypass);
    assert!(state.trusted);
    assert_eq!(state.header.session_id, session_id);
    assert!(state.streaming.is_empty());
    assert!(state.thinking.text.is_empty());
    assert_eq!(state.footer.context_limit, 0);
    // The "cleared" notice is the only remaining (uncommitted) transcript item.
    assert_eq!(state.transcript.len(), 1);
    assert_eq!(state.transcript[0].speaker, "system");
    assert!(state.transcript[0].text.contains("cleared"));
}

#[test]
fn picker_selection_moves_and_closes() {
    let mut state = base();
    state.picker = Some(Picker {
        title: "provider".to_string(),
        options: vec!["local".to_string(), "openai".to_string()],
        selected: 0,
    });
    handle_input(&mut state, AppInput::Key(Key::Down));
    assert_eq!(state.picker.as_ref().unwrap().selected, 1);
    handle_input(&mut state, AppInput::Key(Key::Enter));
    assert!(state.picker.is_none(), "enter closes the picker");
}

#[test]
fn slash_autocomplete_lists_matching_commands_with_descriptions() {
    let mut state = base();
    handle_input(&mut state, AppInput::Key(Key::Char('/')));
    handle_input(&mut state, AppInput::Key(Key::Char('c')));
    let rendered = render_string(&state, 90, 18);
    // Every command starting with "c" is offered, each with its description.
    assert!(rendered.contains("/clear"));
    assert!(rendered.contains("Clear the conversation view"));
    assert!(rendered.contains("/compact"));
    // Non-matching commands are filtered out.
    assert!(!rendered.contains("/agent"));
}

#[test]
fn slash_autocomplete_enter_fills_the_highlighted_command() {
    let mut state = base();
    for c in ['/', 'c', 'l', 'e'] {
        handle_input(&mut state, AppInput::Key(Key::Char(c)));
    }
    // "/cle" matches only "clear"; Enter accepts it into the input and closes.
    handle_input(&mut state, AppInput::Key(Key::Enter));
    assert!(state.slash_picker.is_none());
    assert_eq!(state.input, "/clear");
}
