//! Rendering for the inline TUI.
//!
//! Two surfaces. Finished transcript items become [`Text`] via
//! [`history_block_text`] and are pushed into native scrollback once by the host
//! with `Terminal::insert_before`. The live region — in-progress activity, the
//! composer, and the status line — is the only surface [`render`] redraws each
//! frame, so it snapshot-tests cleanly with a `TestBackend`.

use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;
use tui_widget_list::{ListBuilder, ListState, ListView};

use crate::state::{
    AppState, ApprovalRequest, FilePicker, Header, Picker, Profile, SlashPicker, TranscriptLine,
    TrustPrompt,
};

/// Most text rows the input box grows to before it starts scrolling.
const MAX_INPUT_TEXT_ROWS: u16 = 10;

const SPINNER: [char; 4] = ['◐', '◓', '◑', '◒'];

/// A rounded-border block whose title is padded on the left with a space so the
/// label does not butt against the corner. Centralizing the border style keeps
/// every panel (and the modals) visually consistent.
fn panel(title: impl AsRef<str>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .title(format!(" {}", title.as_ref()))
}

/// Color used for a given speaker in the transcript.
fn speaker_style(speaker: &str) -> Style {
    match speaker {
        "you" => Style::default().fg(Color::Cyan),
        "assistant" => Style::default().fg(Color::Green),
        "tool" => Style::default().fg(Color::DarkGray),
        _ => Style::default().fg(Color::Yellow),
    }
}

/// Render one finished transcript item as styled, wrappable text. The host pushes
/// this into native scrollback once via `Terminal::insert_before`; it is never
/// redrawn, so it carries no scroll or search state.
#[must_use]
pub fn history_block_text(line: &TranscriptLine) -> Text<'static> {
    let style = speaker_style(&line.speaker);
    if line.speaker == "tool" {
        return Text::from(Line::from(vec![
            Span::styled("[tool] ", style.add_modifier(Modifier::ITALIC)),
            Span::styled(line.text.clone(), style),
        ]));
    }
    // Split on newlines so each line gets the speaker prefix (first line) or a
    // continuation indent (subsequent lines), so `\n` in model output renders as
    // real line breaks.
    let mut lines: Vec<Line> = Vec::new();
    for (i, text_line) in line.text.trim_start_matches('\n').split('\n').enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{}: ", line.speaker),
                    style.add_modifier(Modifier::BOLD),
                ),
                Span::styled(text_line.to_string(), style),
            ]));
        } else {
            lines.push(Line::from(Span::styled(format!("  {text_line}"), style)));
        }
    }
    Text::from(lines)
}

/// The one-time session header, printed into scrollback at startup.
#[must_use]
pub fn header_text(header: &Header) -> Text<'static> {
    let mut text = format!(
        "LocalPilot {} | {}/{} | ws:{} | session:{}",
        header.version, header.provider, header.model, header.workspace, header.session_id
    );
    if let Some(update) = &header.update {
        text.push_str(&format!("  ·  update available: {update}"));
    }
    Text::from(Line::styled(
        text,
        Style::default().add_modifier(Modifier::BOLD),
    ))
}

/// Draw the live region: in-progress activity, the composer, and the status line.
/// Finished output lives in native scrollback above this region and is not drawn
/// here.
pub fn render(frame: &mut Frame, state: &AppState) {
    let area = frame.area();
    let input_height = input_box_height(state, area);
    let rows = Layout::vertical([
        Constraint::Min(0),               // live activity tail
        Constraint::Length(input_height), // composer
        Constraint::Length(2),            // status
    ])
    .split(area);

    render_activity(frame, rows[0], state);
    render_input(frame, rows[1], state);
    render_footer(frame, rows[2], state);

    if let Some(approval) = &state.approval {
        render_approval(frame, area, approval, state);
    }
    if let Some(picker) = &state.picker {
        render_picker(frame, area, picker);
    }
    if let Some(slash) = &state.slash_picker {
        // Anchor the autocomplete just above the input box (rows[1]).
        render_slash_picker(frame, area, rows[1], slash);
    }
    if let Some(files) = &state.file_picker {
        render_file_picker(frame, area, rows[1], files);
    }
    // The trust gate draws on top of everything else.
    if let Some(trust) = &state.trust {
        render_trust(frame, area, trust);
    }
}

/// The transient live tail: the model's plan, any running tools, the reasoning
/// panel (when shown), and the in-progress streamed answer — bottom-anchored so
/// the latest rows stay visible. Each item settles into scrollback once it is
/// finished; this surface is for work still in flight.
fn render_activity(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height == 0 {
        return;
    }
    let mut lines: Vec<Line> = Vec::new();

    if !state.plan.is_empty() {
        let done = state.plan.iter().filter(|i| i.status == "done").count();
        lines.push(Line::styled(
            format!("plan ({done}/{})", state.plan.len()),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        for item in &state.plan {
            let (marker, style) = match item.status.as_str() {
                "done" => ("[x]", Style::default().fg(Color::Green)),
                "in_progress" => ("[~]", Style::default().fg(Color::Yellow)),
                _ => ("[ ]", Style::default()),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{marker} "), style),
                Span::raw(item.title.clone()),
            ]));
        }
    }

    for tool in &state.active_tools {
        lines.push(Line::styled(
            format!("⚙ {} running…", tool.name),
            speaker_style("tool").add_modifier(Modifier::ITALIC),
        ));
    }

    if state.thinking.visible && !state.thinking.text.is_empty() {
        for text_line in state.thinking.text.split('\n') {
            lines.push(Line::styled(
                format!("· {text_line}"),
                Style::default().fg(Color::DarkGray),
            ));
        }
    }

    if !state.streaming.is_empty() {
        let style = speaker_style("assistant");
        for (i, text_line) in state
            .streaming
            .trim_start_matches('\n')
            .split('\n')
            .enumerate()
        {
            if i == 0 {
                lines.push(Line::from(vec![
                    Span::styled("assistant: ", style.add_modifier(Modifier::BOLD)),
                    Span::styled(text_line.to_string(), style),
                ]));
            } else {
                lines.push(Line::from(Span::styled(format!("  {text_line}"), style)));
            }
        }
    }

    if lines.is_empty() {
        return;
    }
    let text = Text::from(lines);
    // Bottom-anchor: count wrapped rows with ratatui's own word-wrapping and
    // scroll so the latest rows stay in view when the tail is taller than the
    // region.
    let total = (Paragraph::new(text.clone())
        .wrap(Wrap { trim: false })
        .line_count(area.width) as u16)
        .max(1);
    let scroll = total.saturating_sub(area.height);
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

/// The number of terminal rows a string occupies once wrapped to `width`.
fn wrapped_rows(text: &str, width: u16) -> usize {
    if text.is_empty() {
        return 0;
    }
    let width = width.max(1) as usize;
    text.split('\n')
        .map(|line| {
            let chars = line.chars().count();
            if chars == 0 {
                1
            } else {
                chars.div_ceil(width)
            }
        })
        .sum()
}

/// Height of the bordered input box: it grows with the content up to a cap, then
/// the content scrolls inside a fixed box, never starving the status line.
fn input_box_height(state: &AppState, area: Rect) -> u16 {
    let inner_width = area.width.saturating_sub(2);
    let cursor_rows = input_cursor_position(state, inner_width).0 + 1;
    let text_rows = (wrapped_rows(&state.input, inner_width) as u16)
        .max(cursor_rows)
        .max(1);
    // Leave room for the two-row status line and this box's own two border rows.
    let room = area.height.saturating_sub(2 + 2);
    let cap = room.clamp(1, MAX_INPUT_TEXT_ROWS);
    text_rows.min(cap) + 2
}

fn render_input(frame: &mut Frame, area: Rect, state: &AppState) {
    let title = if state.busy {
        format!(
            "input  {} working {}s  (Ctrl-C to cancel)",
            SPINNER[state.spinner % SPINNER.len()],
            state.working_secs
        )
    } else {
        "input  (Enter sends · Alt+Enter, Ctrl+J, or trailing \\ make a newline)".to_string()
    };
    let inner_width = area.width.saturating_sub(2);
    let (cursor_row, cursor_col) = input_cursor_position(state, inner_width);
    let visible_rows = area.height.saturating_sub(2).max(1);
    let scroll = cursor_row.saturating_add(1).saturating_sub(visible_rows);
    frame.render_widget(
        Paragraph::new(state.input.clone())
            .block(panel(title))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
    if state.trust.is_none() && state.approval.is_none() && state.picker.is_none() {
        frame.set_cursor_position(Position::new(
            area.x.saturating_add(1).saturating_add(cursor_col),
            area.y
                .saturating_add(1)
                .saturating_add(cursor_row.saturating_sub(scroll)),
        ));
    }
}

/// Visual row and column of the UTF-8 input cursor after wrapping.
fn input_cursor_position(state: &AppState, width: u16) -> (u16, u16) {
    let width = width.max(1);
    let mut row = 0u16;
    let mut col = 0u16;
    for ch in state.input[..state.normalized_input_cursor()].chars() {
        if ch == '\n' {
            row = row.saturating_add(1);
            col = 0;
            continue;
        }
        col = col.saturating_add(1);
        if col == width {
            row = row.saturating_add(1);
            col = 0;
        }
    }
    (row, col)
}

fn render_trust(frame: &mut Frame, area: Rect, trust: &TrustPrompt) {
    let popup = centered(area, 72, 11);
    frame.render_widget(Clear, popup);
    let text = Text::from(vec![
        Line::raw("Starting a session in this folder:"),
        Line::raw(""),
        Line::styled(
            trust.path.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::raw("Once trusted, LocalPilot may read, edit, and run commands here"),
        Line::raw("subject to the active permission profile."),
        Line::raw(""),
        Line::raw("[y] trust this folder    [n] exit"),
    ]);
    frame.render_widget(
        Paragraph::new(text)
            .block(panel("trust this folder?"))
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn render_footer(frame: &mut Frame, area: Rect, state: &AppState) {
    let f = &state.footer;
    let context = if f.context_limit > 0 {
        format!("{}/{}", f.context_used, f.context_limit)
    } else {
        "-".to_string()
    };
    let profile_style = if state.profile == Profile::Bypass {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    // The context figure is the bytes/4 estimate against the session budget
    // (the model's real window minus a response reserve when known); the tilde
    // states the basis.
    let effort = f
        .effort
        .as_deref()
        .map(|level| format!(" effort:{level}"))
        .unwrap_or_default();
    let line1 = Line::from(vec![
        Span::raw(format!("mode:{} ", state.mode.label())),
        Span::styled(format!("profile:{} ", state.profile.label()), profile_style),
        Span::raw(format!(
            "tok in/out:{}/{} {:.0} t/s ctx:~{context}{effort}",
            f.tokens_in, f.tokens_out, f.tokens_per_sec
        )),
    ]);
    let mut line2 = format!(
        "F12 mouse:{}",
        if state.mouse_capture {
            "wheel"
        } else {
            "select"
        }
    );
    if let Some(cost) = f.cost_usd {
        line2.push_str(&format!("  est ${cost:.4}"));
    }
    if let Some(reset) = &f.quota_reset {
        line2.push_str(&format!("  quota resets: {reset}"));
    }
    frame.render_widget(
        Paragraph::new(Text::from(vec![line1, Line::raw(line2)])),
        area,
    );
}

fn render_approval(frame: &mut Frame, area: Rect, approval: &ApprovalRequest, state: &AppState) {
    let popup = centered(area, 60, 8);
    frame.render_widget(Clear, popup);
    let text = Text::from(vec![
        Line::raw(format!("tool: {}", approval.tool)),
        Line::raw(format!("target: {}", approval.target)),
        Line::raw(format!("risk: {}", approval.risk_class)),
        Line::raw(format!("profile: {}", state.profile.label())),
        Line::raw(""),
        Line::raw("[y] approve   [n] deny"),
    ]);
    frame.render_widget(Paragraph::new(text).block(panel("approve tool?")), popup);
}

fn render_picker(frame: &mut Frame, area: Rect, picker: &Picker) {
    let popup = centered(area, 50, picker.options.len() as u16 + 2);
    frame.render_widget(Clear, popup);
    let items: Vec<ListItem> = picker
        .options
        .iter()
        .enumerate()
        .map(|(i, opt)| {
            let marker = if i == picker.selected { "> " } else { "  " };
            ListItem::new(format!("{marker}{opt}"))
        })
        .collect();
    frame.render_widget(List::new(items).block(panel(&picker.title)), popup);
}

/// Render the slash-command autocomplete popup just above the input box. Each row
/// shows the command and a short description; the highlighted row is reversed.
fn render_slash_picker(frame: &mut Frame, area: Rect, input_area: Rect, picker: &SlashPicker) {
    if picker.items.is_empty() {
        return;
    }
    // Size the box to the widest rendered " /<name padded to 12><description>"
    // line, capped to the screen. The leading " /" is two columns.
    let content_width = picker
        .items
        .iter()
        .map(|item| 2 + item.name.len().max(12) + item.description.len())
        .max()
        .unwrap_or(20) as u16;
    let popup_width = content_width.saturating_add(2).clamp(12, area.width);
    let visible = (picker.items.len() as u16).min(8);
    let popup_height = visible + 2; // two rows for the border

    // Anchor above the input box, left-aligned with it, clamped into the screen.
    let popup_x = input_area.x.min(area.width.saturating_sub(popup_width));
    let popup_y = input_area.y.saturating_sub(popup_height);
    let popup = Rect::new(popup_x, popup_y, popup_width, popup_height);
    frame.render_widget(Clear, popup);

    let builder = ListBuilder::new(|ctx| {
        let item = &picker.items[ctx.index];
        let line = Line::from(vec![
            Span::styled(
                format!(" /{:<12}", item.name),
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(
                item.description.clone(),
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        let line = if ctx.is_selected {
            line.style(Style::default().add_modifier(Modifier::REVERSED))
        } else {
            line
        };
        (line, 1u16)
    });

    let list = ListView::new(builder, picker.items.len())
        .block(panel("commands"))
        .infinite_scrolling(true);
    let mut list_state = ListState::default();
    list_state.select(Some(picker.selected));
    frame.render_stateful_widget(list, popup, &mut list_state);
}

/// Render the `@` file-mention autocomplete popup just above the input box. Each
/// row shows one workspace-relative path; the highlighted row is reversed.
fn render_file_picker(frame: &mut Frame, area: Rect, input_area: Rect, picker: &FilePicker) {
    if picker.items.is_empty() {
        return;
    }
    // Size the box to the widest " <path>" line, capped to the screen.
    let content_width = picker
        .items
        .iter()
        .map(|item| 1 + item.path.len())
        .max()
        .unwrap_or(20) as u16;
    let popup_width = content_width.saturating_add(2).clamp(12, area.width);
    let visible = (picker.items.len() as u16).min(8);
    let popup_height = visible + 2; // two rows for the border

    let popup_x = input_area.x.min(area.width.saturating_sub(popup_width));
    let popup_y = input_area.y.saturating_sub(popup_height);
    let popup = Rect::new(popup_x, popup_y, popup_width, popup_height);
    frame.render_widget(Clear, popup);

    let builder = ListBuilder::new(|ctx| {
        let item = &picker.items[ctx.index];
        let line = Line::from(Span::styled(
            format!(" {}", item.path),
            Style::default().fg(Color::Cyan),
        ));
        let line = if ctx.is_selected {
            line.style(Style::default().add_modifier(Modifier::REVERSED))
        } else {
            line
        };
        (line, 1u16)
    });

    let list = ListView::new(builder, picker.items.len())
        .block(panel("files"))
        .infinite_scrolling(true);
    let mut list_state = ListState::default();
    list_state.select(Some(picker.selected));
    frame.render_stateful_widget(list, popup, &mut list_state);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Header, Mode, PlanItem, TranscriptLine};

    fn state_with_input(input: &str) -> AppState {
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
        state.input = input.to_string();
        state
    }

    fn buffer_to_string(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
        let buf = terminal.backend().buffer();
        let area = buf.area;
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn input_box_grows_until_the_global_cap() {
        let state = state_with_input("1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12");
        let area = Rect::new(0, 0, 80, 40);
        assert_eq!(input_box_height(&state, area), MAX_INPUT_TEXT_ROWS + 2);
    }

    #[test]
    fn input_box_cap_shrinks_with_terminal_height() {
        // A live region only tall enough for two text rows plus the status line
        // and borders caps the input box to those two rows.
        let state = state_with_input("1\n2\n3\n4\n5\n6");
        let area = Rect::new(0, 0, 80, 6);
        assert_eq!(input_box_height(&state, area), 4);
    }

    #[test]
    fn input_box_counts_wrapped_rows() {
        // 22 chars wrap to three rows at inner width 10, plus two border rows.
        let state = state_with_input("abcdefghijklmnopqrstuv");
        let area = Rect::new(0, 0, 12, 40);
        assert_eq!(input_box_height(&state, area), 5);
    }

    #[test]
    fn cursor_position_tracks_wrapping_and_newlines() {
        let mut state = state_with_input("abcd\nef");
        state.input_cursor = state.input.len();
        assert_eq!(input_cursor_position(&state, 3), (2, 2));
    }

    #[test]
    fn history_block_text_prefixes_the_speaker_and_indents_continuations() {
        let line = TranscriptLine {
            speaker: "assistant".to_string(),
            text: "line one\nline two".to_string(),
        };
        let text = history_block_text(&line);
        let rendered: Vec<String> = text
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
            .collect();
        assert_eq!(rendered, vec!["assistant: line one", "  line two"]);
    }

    #[test]
    fn history_block_text_uses_a_compact_tool_prefix() {
        let line = TranscriptLine {
            speaker: "tool".to_string(),
            text: "read_file ok: hello".to_string(),
        };
        let text = history_block_text(&line);
        let rendered: String = text.lines[0]
            .spans
            .iter()
            .map(|s| s.content.clone())
            .collect();
        assert_eq!(rendered, "[tool] read_file ok: hello");
    }

    #[test]
    fn the_live_region_shows_streaming_and_keeps_the_status_visible() {
        let mut state = state_with_input("");
        state.streaming = "Streaming the answer live...".to_string();
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 8))
            .expect("test terminal");
        terminal
            .draw(|frame| render(frame, &state))
            .expect("render succeeds");
        let rendered = buffer_to_string(&terminal);
        assert!(rendered.contains("assistant: Streaming the answer live..."));
        assert!(rendered.contains("mode:agent"));
    }

    #[test]
    fn an_active_tool_is_shown_as_a_live_indicator() {
        let mut state = state_with_input("");
        state.active_tools.push(crate::state::ActiveTool {
            id: "1".to_string(),
            name: "run_shell".to_string(),
        });
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 8))
            .expect("test terminal");
        terminal
            .draw(|frame| render(frame, &state))
            .expect("render succeeds");
        assert!(buffer_to_string(&terminal).contains("run_shell running"));
    }

    #[test]
    fn the_plan_renders_in_the_live_region() {
        let mut state = state_with_input("");
        state.plan = vec![
            PlanItem {
                title: "first".to_string(),
                status: "done".to_string(),
            },
            PlanItem {
                title: "second".to_string(),
                status: "in_progress".to_string(),
            },
        ];
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 10))
            .expect("test terminal");
        terminal
            .draw(|frame| render(frame, &state))
            .expect("render succeeds");
        assert!(buffer_to_string(&terminal).contains("plan (1/2)"));
    }

    #[test]
    fn busy_input_keeps_the_cursor_visible() {
        let mut state = state_with_input("next");
        state.input_cursor = state.input.len();
        state.busy = true;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 8))
            .expect("test terminal");
        terminal
            .draw(|frame| render(frame, &state))
            .expect("render succeeds");
        assert!(terminal.get_cursor_position().is_ok());
    }
}
