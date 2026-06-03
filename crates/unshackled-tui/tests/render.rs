//! TUI render snapshots and event-loop behaviour.
#![allow(clippy::unwrap_used)]

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;
use unshackled_tui::{
    handle_input, render, run, AppInput, AppState, ApprovalRequest, Header, Key, Mode, Picker,
    Profile, ThinkingPanel, TranscriptLine, TrustPrompt, UiEvent,
};

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
        path: r"D:\repos\rust\unshackled".to_string(),
    });
    let rendered = render_string(&state, 90, 18);
    assert!(rendered.contains(r"D:\repos\rust\unshackled"));
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
