//! Rendering. A single [`render`] draws the whole UI from [`AppState`]; it is
//! pure with respect to the state, so it snapshot-tests cleanly with a
//! `TestBackend`.

use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use crate::state::{AppState, ApprovalRequest, Picker, Profile, TrustPrompt};

/// Below this width the optional side panel collapses; the footer stays visible.
const NARROW_WIDTH: u16 = 80;

/// Most text rows the input box grows to before it starts scrolling.
const MAX_INPUT_TEXT_ROWS: u16 = 10;

/// Draw the entire UI for the current state.
pub fn render(frame: &mut Frame, state: &AppState) {
    let area = frame.area();
    let narrow = area.width < NARROW_WIDTH;

    let input_height = input_box_height(state, area);
    let rows = Layout::vertical([
        Constraint::Length(3),            // header
        Constraint::Min(3),               // body
        Constraint::Length(input_height), // input (grows with content, then scrolls)
        Constraint::Length(2),            // footer (always visible)
    ])
    .split(area);

    render_header(frame, rows[0], state);
    render_body(frame, rows[1], state, narrow);
    render_input(frame, rows[2], state);
    render_footer(frame, rows[3], state);

    if let Some(approval) = &state.approval {
        render_approval(frame, area, approval, state);
    }
    if let Some(picker) = &state.picker {
        render_picker(frame, area, picker);
    }
    // The trust gate draws on top of everything else.
    if let Some(trust) = &state.trust {
        render_trust(frame, area, trust);
    }
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
/// the content scrolls inside a fixed box, never starving the header/body/footer.
fn input_box_height(state: &AppState, area: Rect) -> u16 {
    let inner_width = area.width.saturating_sub(2);
    let cursor_rows = input_cursor_position(state, inner_width).0 + 1;
    let text_rows = (wrapped_rows(&state.input, inner_width) as u16)
        .max(cursor_rows)
        .max(1);
    // Leave room for header (3), footer (2), the body minimum (3), and this box's
    // own two border rows.
    let room = area.height.saturating_sub(3 + 2 + 3 + 2);
    let cap = room.clamp(1, MAX_INPUT_TEXT_ROWS);
    text_rows.min(cap) + 2
}

fn render_header(frame: &mut Frame, area: Rect, state: &AppState) {
    let h = &state.header;
    let mut text = format!(
        "Unshackled v{} | {}/{} | ws:{} | session:{}",
        h.version, h.provider, h.model, h.workspace, h.session_id
    );
    if let Some(update) = &h.update {
        text.push_str(&format!("  ·  update available: {update}"));
    }
    frame.render_widget(
        Paragraph::new(text).block(Block::bordered().title("Unshackled")),
        area,
    );
}

fn render_body(frame: &mut Frame, area: Rect, state: &AppState, narrow: bool) {
    // A task-plan panel sits above the transcript when the model has set a plan.
    let area = if state.plan.is_empty() {
        area
    } else {
        let plan_height = (state.plan.len() as u16 + 2).min(area.height / 2);
        let rows =
            Layout::vertical([Constraint::Length(plan_height), Constraint::Min(3)]).split(area);
        render_plan(frame, rows[0], state);
        rows[1]
    };

    if state.thinking.visible && !narrow {
        let cols = Layout::horizontal([Constraint::Min(20), Constraint::Length(30)]).split(area);
        render_transcript(frame, cols[0], state);
        render_thinking(frame, cols[1], state);
    } else {
        render_transcript(frame, area, state);
    }
}

fn render_plan(frame: &mut Frame, area: Rect, state: &AppState) {
    let lines: Vec<Line> = state
        .plan
        .iter()
        .map(|item| {
            let (marker, style) = match item.status.as_str() {
                "done" => ("[x]", Style::default().fg(Color::Green)),
                "in_progress" => ("[~]", Style::default().fg(Color::Yellow)),
                _ => ("[ ]", Style::default()),
            };
            Line::from(vec![
                Span::styled(format!("{marker} "), style),
                Span::raw(item.title.clone()),
            ])
        })
        .collect();
    let done = state.plan.iter().filter(|i| i.status == "done").count();
    let title = format!("plan ({done}/{})", state.plan.len());
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(Block::bordered().title(title)),
        area,
    );
}

fn render_transcript(frame: &mut Frame, area: Rect, state: &AppState) {
    let mut lines: Vec<Line> = Vec::new();
    for entry in &state.transcript {
        let matched = state
            .search
            .as_deref()
            .is_some_and(|q| !q.is_empty() && entry.text.contains(q));
        let prefix = if matched { ">" } else { " " };
        // Split on newlines so each line gets the speaker prefix (first line) or
        // a continuation indent (subsequent lines).  This makes `\n` in model
        // output actually render as line breaks instead of being swallowed.
        for (i, text_line) in entry.text.trim_start_matches('\n').split('\n').enumerate() {
            if i == 0 {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{prefix}{}: ", entry.speaker),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(text_line.to_string()),
                ]));
            } else {
                // Continuation lines get a two-space indent to visually align
                // with the text after "speaker: ".
                lines.push(Line::from(Span::raw(format!("  {text_line}"))));
            }
        }
    }
    if !state.streaming.is_empty() {
        for (i, text_line) in state
            .streaming
            .trim_start_matches('\n')
            .split('\n')
            .enumerate()
        {
            if i == 0 {
                lines.push(Line::from(vec![
                    Span::styled("assistant: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(text_line.to_string()),
                ]));
            } else {
                lines.push(Line::from(Span::raw(format!("  {text_line}"))));
            }
        }
    }
    let title = match &state.search {
        Some(q) if !q.is_empty() => format!("transcript [search: {q}]"),
        _ => "transcript".to_string(),
    };
    let inner_width = area.width.saturating_sub(2).max(1) as usize;
    let total_rows: usize = lines
        .iter()
        .map(|line| {
            let width = line.width();
            if width == 0 {
                1
            } else {
                width.div_ceil(inner_width)
            }
        })
        .sum();
    let paragraph = Paragraph::new(Text::from(lines))
        .block(Block::bordered().title(title))
        .wrap(Wrap { trim: false });
    let visible_rows = area.height.saturating_sub(2).max(1) as usize;
    let scroll = u16::try_from(total_rows.saturating_sub(visible_rows)).unwrap_or(u16::MAX);
    frame.render_widget(paragraph.scroll((scroll, 0)), area);
}

fn render_thinking(frame: &mut Frame, area: Rect, state: &AppState) {
    frame.render_widget(
        Paragraph::new(state.thinking.text.clone())
            .block(Block::bordered().title("thinking"))
            .style(Style::default().fg(Color::DarkGray))
            .wrap(Wrap { trim: false }),
        area,
    );
}

const SPINNER: [char; 4] = ['|', '/', '-', '\\'];

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
            .block(Block::bordered().title(title))
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
        Line::raw("Once trusted, Unshackled may read, edit, and run commands here"),
        Line::raw("subject to the active permission profile."),
        Line::raw(""),
        Line::raw("[y] trust this folder    [n] exit"),
    ]);
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::bordered().title("trust this folder?"))
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

    let line1 = Line::from(vec![
        Span::raw(format!("mode:{} ", state.mode.label())),
        Span::styled(format!("profile:{} ", state.profile.label()), profile_style),
        Span::raw(format!(
            "tok in/out:{}/{} {:.0} t/s ctx:{context}",
            f.tokens_in, f.tokens_out, f.tokens_per_sec
        )),
    ]);
    let mut line2 = String::new();
    if let Some(cost) = f.cost_usd {
        line2.push_str(&format!("est ${cost:.4}  "));
    }
    if let Some(reset) = &f.quota_reset {
        line2.push_str(&format!("quota resets: {reset}"));
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
    frame.render_widget(
        Paragraph::new(text).block(Block::bordered().title("approve tool?")),
        popup,
    );
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
    frame.render_widget(
        List::new(items).block(Block::bordered().title(picker.title.clone())),
        popup,
    );
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
    use crate::state::{Header, Mode};

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

    #[test]
    fn input_box_grows_until_the_global_cap() {
        let state = state_with_input("1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12");
        let area = Rect::new(0, 0, 80, 40);
        assert_eq!(input_box_height(&state, area), MAX_INPUT_TEXT_ROWS + 2);
    }

    #[test]
    fn input_box_cap_shrinks_with_terminal_height() {
        let state = state_with_input("1\n2\n3\n4\n5\n6");
        let area = Rect::new(0, 0, 80, 13);
        assert_eq!(input_box_height(&state, area), 5);
    }

    #[test]
    fn input_box_counts_wrapped_rows() {
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
    fn busy_input_keeps_the_cursor_visible() {
        let mut state = state_with_input("next");
        state.input_cursor = state.input.len();
        state.busy = true;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20))
            .expect("test terminal");
        terminal
            .draw(|frame| render(frame, &state))
            .expect("render succeeds");
        assert!(terminal.get_cursor_position().is_ok());
    }
}
