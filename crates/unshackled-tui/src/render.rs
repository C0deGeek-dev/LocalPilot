//! Rendering. A single [`render`] draws the whole UI from [`AppState`]; it is
//! pure with respect to the state, so it snapshot-tests cleanly with a
//! `TestBackend`.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use crate::state::{AppState, ApprovalRequest, Picker, Profile};

/// Below this width the optional side panel collapses; the footer stays visible.
const NARROW_WIDTH: u16 = 80;

/// Draw the entire UI for the current state.
pub fn render(frame: &mut Frame, state: &AppState) {
    let area = frame.area();
    let narrow = area.width < NARROW_WIDTH;

    let rows = Layout::vertical([
        Constraint::Length(3), // header
        Constraint::Min(3),    // body
        Constraint::Length(3), // input
        Constraint::Length(2), // footer (always visible)
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
        lines.push(Line::from(vec![
            Span::styled(
                format!("{prefix}{}: ", entry.speaker),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(entry.text.clone()),
        ]));
    }
    if !state.streaming.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("assistant: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(state.streaming.clone()),
        ]));
    }
    let title = match &state.search {
        Some(q) if !q.is_empty() => format!("transcript [search: {q}]"),
        _ => "transcript".to_string(),
    };
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::bordered().title(title))
            .wrap(Wrap { trim: false }),
        area,
    );
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
        "input  (Enter to send · Alt+Enter for newline)".to_string()
    };
    frame.render_widget(
        Paragraph::new(state.input.clone())
            .block(Block::bordered().title(title))
            .wrap(Wrap { trim: false }),
        area,
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
