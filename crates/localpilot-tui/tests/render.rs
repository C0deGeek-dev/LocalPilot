//! TUI render snapshots and event-loop behaviour.
#![allow(clippy::unwrap_used)]

use localpilot_tui::{
    handle_input, parse_slash, render, run, AppInput, AppState, ApprovalRequest, Header, Key, Mode,
    Picker, Profile, SlashAction, ThinkingPanel, TranscriptLine, TrustPrompt, UiEvent,
};
use ratatui::backend::{Backend, TestBackend};
use ratatui::buffer::Buffer;
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
    state.transcript.push(TranscriptLine {
        speaker: "you".to_string(),
        text: "summarize the parser".to_string(),
    });
    state.transcript.push(TranscriptLine {
        speaker: "assistant".to_string(),
        text: "The parser reports precise errors.".to_string(),
    });
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
    terminal.draw(|frame| render(frame, state)).unwrap();
    buffer_string(terminal.backend().buffer())
}

#[test]
fn full_layout_snapshot() {
    insta::assert_snapshot!(render_string(&base(), 90, 18));
}

#[test]
fn streaming_turn_snapshot() {
    let mut state = base();
    state.streaming = "Streaming the answer live...".to_string();
    insta::assert_snapshot!(render_string(&state, 90, 18));
}

#[test]
fn thinking_panel_on_does_not_occlude_the_footer() {
    let mut state = base();
    state.thinking = ThinkingPanel {
        visible: true,
        text: "considering the edge cases".to_string(),
    };
    let rendered = render_string(&state, 90, 18);
    insta::assert_snapshot!(rendered);
    // The footer's mode/profile line is still present below the panel.
    assert!(rendered.contains("mode:agent"));
    assert!(rendered.contains("thinking"));
}

#[test]
fn approval_modal_snapshot() {
    let mut state = base();
    state.approval = Some(ApprovalRequest {
        tool: "run_shell".to_string(),
        target: "rm -rf build".to_string(),
        risk_class: "destructive".to_string(),
    });
    insta::assert_snapshot!(render_string(&state, 90, 18));
}

#[test]
fn trust_modal_shows_the_full_workspace_path() {
    let mut state = base();
    state.trust = Some(TrustPrompt {
        path: r"D:\repos\rust\localpilot".to_string(),
    });
    let rendered = render_string(&state, 90, 18);
    assert!(rendered.contains(r"D:\repos\rust\localpilot"));
    assert!(rendered.contains("trust this folder?"));
}

#[test]
fn bypass_profile_is_visible_in_the_footer() {
    let mut state = base();
    state.profile = Profile::Bypass;
    let rendered = render_string(&state, 90, 8);
    assert!(rendered.contains("profile:BYPASS"));
}

#[test]
fn narrow_collapses_panel_but_keeps_footer() {
    let mut state = base();
    state.thinking = ThinkingPanel {
        visible: true,
        text: "hidden when narrow".to_string(),
    };
    let narrow = render_string(&state, 50, 18);
    // The side panel is collapsed at narrow widths...
    assert!(!narrow.contains("hidden when narrow"));
    // ...but the footer stats remain.
    assert!(narrow.contains("mode:agent"));

    let wide = render_string(&state, 100, 18);
    assert!(wide.contains("hidden when narrow"));
    assert!(wide.contains("mode:agent"));
}

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
    assert_eq!(parse_slash("/resume"), Some(SlashAction::Resume));
    assert_eq!(parse_slash("/wait-resume"), Some(SlashAction::WaitResume));
    assert_eq!(parse_slash("/wait_resume"), Some(SlashAction::WaitResume));
}

#[test]
fn clear_compact_and_search_slash_commands_are_parsed() {
    assert_eq!(parse_slash("/clear"), Some(SlashAction::Clear));
    assert_eq!(parse_slash("/compact"), Some(SlashAction::Compact));
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
fn search_command_sets_and_clears_search_without_changing_transcript() {
    let mut state = base();
    let original = state.transcript.clone();

    state.input = "/search parser".to_string();
    state.input_cursor = state.input.len();
    handle_input(&mut state, AppInput::Key(Key::Enter));

    assert_eq!(state.search, Some("parser".to_string()));
    assert_eq!(state.transcript.len(), original.len());
    assert_eq!(state.transcript[0].text, original[0].text);
    assert!(render_string(&state, 90, 18).contains("search: parser"));

    state.input = "/search".to_string();
    state.input_cursor = state.input.len();
    handle_input(&mut state, AppInput::Key(Key::Enter));

    assert!(state.search.is_none());
    assert_eq!(state.transcript.len(), original.len());
}

#[test]
fn clear_command_resets_conversation_view_but_keeps_session_settings() {
    let mut state = base();
    state.mode = Mode::Harness;
    state.profile = Profile::Bypass;
    state.trusted = true;
    state.search = Some("parser".to_string());
    state.streaming = "partial".to_string();
    state.thinking.text = "reasoning".to_string();
    state.plan = vec![localpilot_tui::PlanItem {
        title: "step".to_string(),
        status: "in_progress".to_string(),
    }];
    let session_id = state.header.session_id.clone();

    state.input = "/clear".to_string();
    state.input_cursor = state.input.len();
    handle_input(&mut state, AppInput::Key(Key::Enter));

    assert_eq!(state.mode, Mode::Harness);
    assert_eq!(state.profile, Profile::Bypass);
    assert!(state.trusted);
    assert_eq!(state.header.session_id, session_id);
    assert!(state.streaming.is_empty());
    assert!(state.search.is_none());
    assert!(state.thinking.text.is_empty());
    assert!(state.plan.is_empty());
    assert_eq!(state.footer.context_limit, 0);
    assert_eq!(state.transcript.len(), 1);
    assert_eq!(state.transcript[0].speaker, "system");
    assert!(state.transcript[0].text.contains("cleared"));
}

#[test]
fn transcript_splits_multiline_assistant_responses() {
    let mut state = base();
    // Replace the single-line assistant message with a multiline one.
    state.transcript.clear();
    state.transcript.push(TranscriptLine {
        speaker: "you".to_string(),
        text: "hello".to_string(),
    });
    state.transcript.push(TranscriptLine {
        speaker: "assistant".to_string(),
        text: "line one\nline two\nline three".to_string(),
    });
    let rendered = render_string(&state, 90, 18);
    // Each line should appear on its own row.
    assert!(rendered.contains("you: hello"));
    assert!(rendered.contains("assistant: line one"));
    assert!(rendered.contains("  line two"));
    assert!(rendered.contains("  line three"));
}

#[test]
fn streaming_text_splits_on_newlines() {
    let mut state = base();
    state.streaming = "first\nsecond\nthird".to_string();
    let rendered = render_string(&state, 90, 18);
    assert!(rendered.contains("assistant: first"));
    assert!(rendered.contains("  second"));
    assert!(rendered.contains("  third"));
}

#[test]
fn tool_transcript_lines_use_compact_prefix() {
    let mut state = base();
    state.transcript.push(TranscriptLine {
        speaker: "tool".to_string(),
        text: "read_file ok: hello world".to_string(),
    });

    let rendered = render_string(&state, 90, 18);
    assert!(rendered.contains("[tool] read_file ok: hello world"));
    assert!(!rendered.contains("tool: read_file"));
}

#[test]
fn picker_selection_moves_and_search_highlights() {
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

    // Search highlights a matching transcript line.
    state.search = Some("parser".to_string());
    let rendered = render_string(&state, 90, 18);
    assert!(rendered.contains("search: parser"));
}

#[test]
fn input_cursor_is_visible_at_the_edit_position() {
    let mut state = base();
    state.input = "abcd".to_string();
    state.input_cursor = 2;
    let mut terminal = Terminal::new(TestBackend::new(90, 18)).unwrap();
    terminal.draw(|frame| render(frame, &state)).unwrap();

    // Input box starts at row 13 in this layout; its content starts at x=1,y=14.
    assert_eq!(
        terminal.backend_mut().get_cursor_position().unwrap(),
        ratatui::layout::Position::new(3, 14)
    );
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

#[test]
fn transcript_follows_the_latest_response_rows() {
    let mut state = base();
    state.transcript.clear();
    state.streaming = (1..=20)
        .map(|line| format!("response line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let rendered = render_string(&state, 60, 12);

    // Following the bottom shows the latest rows; the earliest scroll off the
    // top. The scroll view's scrollbar replaces the old textual position label.
    assert!(rendered.contains("response line 20"));
    assert!(!rendered.contains("response line 1 "));
}

#[test]
fn transcript_page_keys_scroll_the_output_viewport() {
    let mut state = base();
    state.transcript.clear();
    state.streaming = (1..=20)
        .map(|line| format!("response line {line}"))
        .collect::<Vec<_>>()
        .join("\n");

    handle_input(&mut state, AppInput::Key(Key::PageUp));
    let scrolled = render_string(&state, 60, 12);
    // Paging up scrolls earlier rows into the viewport and pushes the latest
    // line out of view. The scroll view's scrollbar replaces the old position
    // label, so the assertion is on the visible content rather than that label.
    assert!(!scrolled.contains("response line 20"));
    assert!(scrolled.contains("response line 10 "));

    handle_input(&mut state, AppInput::Key(Key::PageDown));
    let bottom = render_string(&state, 60, 12);
    // Paging back down returns to following the latest output.
    assert!(bottom.contains("response line 20"));
    assert!(!bottom.contains("response line 10 "));
}
