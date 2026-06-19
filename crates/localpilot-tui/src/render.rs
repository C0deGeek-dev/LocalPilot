//! Rendering for the inline TUI.
//!
//! Two surfaces. Finished transcript items become [`Text`] via
//! [`history_block_text`] (and the launch [`banner_text`]) and are pushed into
//! native scrollback once by the host with `Terminal::insert_before`. The live
//! region — a top section (a blocking prompt, the autocomplete list, or the
//! in-progress activity tail), the composer, and the status line — is the only
//! surface [`render`] redraws each frame, sized by [`live_region_height`] so its
//! content never clips. Nothing floats: there are no centered modals.

use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::state::{
    AppState, ApprovalRequest, BackgroundProcess, FilePicker, Header, MemoryPanel, Profile,
    SlashPicker, TranscriptLine, TrustPrompt,
};

/// Most text rows the input box grows to before it starts scrolling.
const MAX_INPUT_TEXT_ROWS: u16 = 10;

/// Most rows the live activity tail grows the region by before it scrolls
/// internally; keeps a long stream from pushing the composer off-screen.
const MAX_ACTIVITY_ROWS: u16 = 6;

/// Most rows the autocomplete list shows at once before it windows around the
/// selection.
const MAX_PICKER_ROWS: u16 = 8;

/// The two-row status line at the bottom of the live region.
const STATUS_ROWS: u16 = 2;

const SPINNER: [char; 4] = ['◐', '◓', '◑', '◒'];

/// Size of the shimmer placeholder shown where the reply will appear while the
/// model is working and nothing has streamed yet. Rows are Braille cells (each a
/// 2×4 dot grid), so a couple of text rows read as a dense, tightly-stacked field
/// of dots rather than widely-spaced single-dot rows.
const SHIMMER_WIDTH: usize = 20;
const SHIMMER_ROWS: usize = 2;

/// The terminal-monitor mark from the project README, padded to a uniform width
/// so the banner text aligns beside it.
const LOGO: [&str; 5] = [
    "╔══════╗ ╔══╗  ",
    "║ >_ █ ║ ║██║║ ",
    "╚══╦═══╝ ║██║║ ",
    " ══╩══   ╚══╝║ ",
    "═════════════╝",
];

/// A rounded-border block whose title is padded on the left with a space so the
/// label does not butt against the corner.
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

/// The launch banner: the README monitor mark beside the session identity, in
/// color. The host prints it once into scrollback at startup.
#[must_use]
pub fn banner_text(header: &Header) -> Text<'static> {
    let mark = Style::default().fg(Color::Cyan);
    let name = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    let gutter = "   ";

    // Identity text sits beside the middle three rows of the five-row mark.
    let beside: [Vec<Span>; 5] = [
        vec![],
        vec![
            Span::raw(gutter),
            Span::styled("LocalPilot", name),
            Span::styled(
                // The embedded version may already carry a leading `v` (from a
                // `git describe` tag); strip it so we never render `vv…`.
                format!(
                    "  ·  v{}",
                    header.version.strip_prefix('v').unwrap_or(&header.version)
                ),
                dim,
            ),
        ],
        vec![
            Span::raw(gutter),
            Span::styled(format!("{}/{}", header.provider, header.model), dim),
        ],
        vec![
            Span::raw(gutter),
            Span::styled(
                format!("ws:{} · session:{}", header.workspace, header.session_id),
                dim,
            ),
        ],
        vec![],
    ];

    let mut lines: Vec<Line> = Vec::new();
    for (mark_row, extra) in LOGO.iter().zip(beside) {
        let mut spans = vec![Span::styled((*mark_row).to_string(), mark)];
        spans.extend(extra);
        lines.push(Line::from(spans));
    }
    if let Some(update) = &header.update {
        lines.push(Line::styled(
            format!("   update available: {update}"),
            Style::default().fg(Color::Yellow),
        ));
    }
    Text::from(lines)
}

/// Draw the live region: a top section (a blocking prompt, the autocomplete list,
/// or the in-progress activity tail), the composer, and the status line. Finished
/// output lives in native scrollback above this region and is not drawn here.
pub fn render(frame: &mut Frame, state: &AppState) {
    let area = frame.area();
    let input_height = input_box_height(state, area);
    let rows = Layout::vertical([
        Constraint::Min(0),               // top section
        Constraint::Length(input_height), // composer
        Constraint::Length(STATUS_ROWS),  // status
    ])
    .split(area);

    render_top(frame, rows[0], state);
    render_input(frame, rows[1], state);
    render_status(frame, rows[2], state);

    // The memory inspector overlays the top section when shown, so "memories
    // used this turn" can be inspected on demand without leaving the session.
    if state.memory_panel.visible {
        render_memory_panel(frame, rows[0], &state.memory_panel);
    }
}

/// Render the "memories used this turn" inspector over `area`. The host supplies
/// the rendered body; this only frames and wraps it.
fn render_memory_panel(frame: &mut Frame, area: Rect, memory_panel: &MemoryPanel) {
    let body = if memory_panel.body.is_empty() {
        "No memories were recorded as used for the last turn."
    } else {
        memory_panel.body.as_str()
    };
    let lines: Vec<Line> = body
        .split('\n')
        .map(|line| Line::raw(line.to_string()))
        .collect();
    let widget = Paragraph::new(Text::from(lines))
        .block(panel("Memories used this turn  (esc to close)"))
        .wrap(Wrap { trim: false });
    frame.render_widget(Clear, area);
    frame.render_widget(widget, area);
}

// --- Top section --------------------------------------------------------------

/// Lines for the blocking trust gate.
fn trust_lines(trust: &TrustPrompt) -> Vec<Line<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    vec![
        Line::styled("Trust this folder?", bold),
        Line::styled(trust.path.clone(), Style::default().fg(Color::Cyan)),
        Line::raw("Once trusted, LocalPilot may read, edit, and run commands here"),
        Line::raw("subject to the active permission profile."),
        Line::styled("[y] trust this folder    [n] exit", bold),
    ]
}

/// Lines for a pending tool approval.
fn approval_lines(approval: &ApprovalRequest, profile: &str) -> Vec<Line<'static>> {
    let warn = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    vec![
        Line::styled("Approve tool?", warn),
        Line::raw(format!(
            "tool: {}  ({})",
            approval.tool, approval.risk_class
        )),
        Line::raw(format!("target: {}", approval.target)),
        Line::raw(format!("profile: {profile}")),
        Line::styled("[y] approve    [n] deny", warn),
    ]
}

/// The visible window of a list so `selected` stays in view, capped to `max` rows.
fn window(len: usize, selected: usize, max: usize) -> std::ops::Range<usize> {
    if len <= max {
        return 0..len;
    }
    let start = selected.saturating_sub(max / 2).min(len - max);
    start..start + max
}

/// Lines for the slash-command autocomplete list.
fn slash_lines(picker: &SlashPicker) -> Vec<Line<'static>> {
    let range = window(
        picker.items.len(),
        picker.selected,
        MAX_PICKER_ROWS as usize,
    );
    range
        .map(|i| {
            let item = &picker.items[i];
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
            if i == picker.selected {
                line.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                line
            }
        })
        .collect()
}

/// Lines for the `@` file-mention autocomplete list.
fn file_lines(picker: &FilePicker) -> Vec<Line<'static>> {
    let range = window(
        picker.items.len(),
        picker.selected,
        MAX_PICKER_ROWS as usize,
    );
    range
        .map(|i| {
            let line = Line::from(Span::styled(
                format!(" {}", picker.items[i].path),
                Style::default().fg(Color::Cyan),
            ));
            if i == picker.selected {
                line.style(Style::default().add_modifier(Modifier::REVERSED))
            } else {
                line
            }
        })
        .collect()
}

/// The top section's content for the current state, in priority order: a blocking
/// prompt, then the open autocomplete list, then the in-progress activity tail.
fn top_lines(state: &AppState) -> Vec<Line<'static>> {
    if let Some(trust) = &state.trust {
        trust_lines(trust)
    } else if let Some(approval) = &state.approval {
        approval_lines(approval, state.profile.label())
    } else if let Some(slash) = &state.slash_picker {
        slash_lines(slash)
    } else if let Some(files) = &state.file_picker {
        file_lines(files)
    } else {
        activity_lines(state)
    }
}

/// Rows the top section needs at `width`, capped per content type so a long
/// stream or command list cannot push the composer off-screen.
fn top_section_height(state: &AppState, width: u16) -> u16 {
    let lines = top_lines(state);
    if lines.is_empty() {
        return 0;
    }
    let rows = Paragraph::new(Text::from(lines.clone()))
        .wrap(Wrap { trim: false })
        .line_count(width) as u16;
    // Prompts must show in full; lists and the activity tail are capped.
    let cap = if state.trust.is_some() || state.approval.is_some() {
        lines.len() as u16
    } else if state.slash_picker.is_some() || state.file_picker.is_some() {
        MAX_PICKER_ROWS
    } else {
        MAX_ACTIVITY_ROWS
    };
    rows.clamp(1, cap)
}

/// Draw the top section. The activity tail is bottom-anchored (latest rows stay
/// visible); prompts and lists render top-down.
fn render_top(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height == 0 {
        return;
    }
    let lines = top_lines(state);
    if lines.is_empty() {
        return;
    }
    let text = Text::from(lines);
    let total = (Paragraph::new(text.clone())
        .wrap(Wrap { trim: false })
        .line_count(area.width) as u16)
        .max(1);
    // Only the streaming activity tail follows the bottom; prompts/lists pin top.
    let follows_bottom =
        state.trust.is_none() && state.approval.is_none() && !is_autocomplete_open(state);
    let scroll = if follows_bottom {
        total.saturating_sub(area.height)
    } else {
        0
    };
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

fn is_autocomplete_open(state: &AppState) -> bool {
    state.slash_picker.is_some() || state.file_picker.is_some()
}

/// The transient live tail as styled lines: the model's plan, any running tools,
/// the reasoning panel (when shown), and the in-progress streamed answer.
fn activity_lines(state: &AppState) -> Vec<Line<'static>> {
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

    // Waiting for the reply: a shimmer placeholder where the answer will appear,
    // until the first tokens stream in (then `streaming` is no longer empty).
    if state.busy && state.streaming.is_empty() {
        lines.extend(shimmer_lines(state.spinner));
    }

    lines
}

/// A skeleton "shimmer" placeholder: `SHIMMER_ROWS` rows of `SHIMMER_WIDTH` grey
/// Braille dot-cells with a white crest that sweeps across, animated by `tick`
/// (the host's per-frame spinner counter). Each row is phase-shifted so the crest
/// leans into a diagonal wave.
fn shimmer_lines(tick: usize) -> Vec<Line<'static>> {
    // A fully-lit Braille cell: a 2×4 block of dots, so the rows stack densely.
    const DOT: char = '⣿';
    // Grey values: resting dots sit at BASE, the crest peaks at PEAK, fading over
    // HALF columns on either side.
    const BASE: u16 = 80;
    const PEAK: u16 = 255;
    const HALF: f32 = 3.0;

    // The crest travels the full width plus a gap, then repeats.
    let cycle = (SHIMMER_WIDTH + 8) as i32;
    let crest = (tick % cycle as usize) as i32;

    (0..SHIMMER_ROWS)
        .map(|y| {
            // Lean the crest across rows so the wave reads as a diagonal sweep.
            let center = crest - (y as i32) * 2;
            let spans = (0..SHIMMER_WIDTH)
                .map(|x| {
                    let distance = (x as i32 - center).unsigned_abs() as f32;
                    let t = (1.0 - distance / HALF).max(0.0);
                    let v = (BASE + ((PEAK - BASE) as f32 * t) as u16).min(255) as u8;
                    Span::styled(DOT.to_string(), Style::default().fg(Color::Rgb(v, v, v)))
                })
                .collect::<Vec<_>>();
            Line::from(spans)
        })
        .collect()
}

// --- Composer -----------------------------------------------------------------

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

/// Text rows the composer needs for its current content at `width`, clamped to
/// `[1, MAX_INPUT_TEXT_ROWS]`. Beyond the cap the content scrolls inside the box.
fn composer_rows(state: &AppState, width: u16) -> u16 {
    let inner_width = width.saturating_sub(2);
    let cursor_rows = input_cursor_position(state, inner_width).0 + 1;
    (wrapped_rows(&state.input, inner_width) as u16)
        .max(cursor_rows)
        .clamp(1, MAX_INPUT_TEXT_ROWS)
}

/// Height of the bordered input box (composer rows + two borders), additionally
/// capped so it never starves the status line in a small render area.
fn input_box_height(state: &AppState, area: Rect) -> u16 {
    let room = area.height.saturating_sub(STATUS_ROWS + 2);
    let cap = room.clamp(1, MAX_INPUT_TEXT_ROWS);
    composer_rows(state, area.width).min(cap) + 2
}

/// The natural height of the whole live region for the current state at `width`:
/// the top section, the composer box, and the status line. The host sizes the
/// inline viewport to this and re-inits the terminal when it changes, since
/// ratatui has no in-place inline-viewport-height setter.
#[must_use]
pub fn live_region_height(state: &AppState, width: u16) -> u16 {
    top_section_height(state, width) + composer_rows(state, width) + 2 + STATUS_ROWS
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
    // Show the edit cursor unless a blocking y/n prompt owns the keyboard.
    if state.trust.is_none() && state.approval.is_none() {
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

// --- Status -------------------------------------------------------------------

fn render_status(frame: &mut Frame, area: Rect, state: &AppState) {
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

    // The banner scrolls away, so the status line keeps the model and a short
    // session id always visible.
    let short_session = state
        .header
        .session_id
        .get(..8)
        .unwrap_or(state.header.session_id.as_str());
    let mut line2 = format!("{} · session:{short_session}", state.header.model);
    if let Some(cost) = f.cost_usd {
        line2.push_str(&format!("  est ${cost:.4}"));
    }
    if let Some(reset) = &f.quota_reset {
        line2.push_str(&format!("  quota resets: {reset}"));
    }
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            line1,
            Line::styled(line2, Style::default().fg(Color::DarkGray)),
        ])),
        area,
    );

    // Background processes are pinned to the bottom-right corner so a running
    // dev server stays visible at a glance, separate from the per-turn stats.
    render_background_indicator(frame, area, state);
}

/// Render a right-aligned background-process indicator on the bottom status row:
/// the command when a single one runs, otherwise a running count. Hidden when
/// nothing is running.
fn render_background_indicator(frame: &mut Frame, area: Rect, state: &AppState) {
    if area.height < STATUS_ROWS {
        return;
    }
    let running: Vec<&BackgroundProcess> = state.background.iter().filter(|p| p.alive).collect();
    if running.is_empty() {
        return;
    }
    let label = if running.len() == 1 {
        format!("▶ {}", truncate_end(&running[0].command, 32))
    } else {
        format!("▶ {} background procs", running.len())
    };
    let bottom = Rect {
        x: area.x,
        y: area.y + STATUS_ROWS - 1,
        width: area.width,
        height: 1,
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            label,
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )))
        .alignment(Alignment::Right),
        bottom,
    );
}

/// Truncate `text` to `max` characters, appending an ellipsis when shortened.
fn truncate_end(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ActiveTool, Header, Mode, SlashSuggestion, TranscriptLine};

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

    /// Render at the state's own natural live-region height — the size the host
    /// gives the inline viewport — so tests see what the user sees.
    fn render_natural(state: &AppState, width: u16) -> String {
        let height = live_region_height(state, width).max(1);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, state)).unwrap();
        buffer_to_string(&terminal)
    }

    #[test]
    fn memory_panel_renders_the_inspector_body_when_visible() {
        let mut state = state_with_input("");
        state.memory_panel.visible = true;
        state.memory_panel.body =
            "Memories used this turn:\n- [memory] mem-1 (score 42)\n    status: decision · confidence: 0.80 · active"
                .to_string();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(70, 12)).unwrap();
        terminal.draw(|frame| render(frame, &state)).unwrap();
        let rendered = buffer_to_string(&terminal);
        assert!(rendered.contains("Memories used this turn"), "{rendered}");
        assert!(rendered.contains("mem-1"), "{rendered}");
        assert!(rendered.contains("decision"), "{rendered}");
    }

    #[test]
    fn memory_panel_is_hidden_when_not_visible() {
        let state = state_with_input("");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(70, 12)).unwrap();
        terminal.draw(|frame| render(frame, &state)).unwrap();
        assert!(!buffer_to_string(&terminal).contains("Memories used this turn"));
    }

    #[test]
    fn input_box_grows_until_the_global_cap() {
        let state = state_with_input("1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12");
        let area = Rect::new(0, 0, 80, 40);
        assert_eq!(input_box_height(&state, area), MAX_INPUT_TEXT_ROWS + 2);
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
    fn history_block_text_prefixes_the_speaker_and_indents_continuations() {
        let line = TranscriptLine {
            speaker: "assistant".to_string(),
            text: "line one\nline two".to_string(),
        };
        let rendered: Vec<String> = history_block_text(&line)
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
            .collect();
        assert_eq!(rendered, vec!["assistant: line one", "  line two"]);
    }

    #[test]
    fn the_banner_carries_the_logo_and_identity() {
        let mut state = state_with_input("");
        state.header.version = "9.9".into();
        state.header.session_id = "abcd1234ef".into();
        let rendered: String = banner_text(&state.header)
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("LocalPilot"));
        assert!(rendered.contains("v9.9"));
        assert!(rendered.contains("session:abcd1234ef"));
        assert!(rendered.contains("╔══════╗")); // the monitor mark
    }

    #[test]
    fn the_banner_does_not_double_the_version_v_prefix() {
        let mut state = state_with_input("");
        // A `git describe` tag already carries a leading `v`.
        state.header.version = "v0.3.0-beta.2".into();
        let rendered: String = banner_text(&state.header)
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("v0.3.0-beta.2"));
        assert!(!rendered.contains("vv0.3.0-beta.2"));
    }

    #[test]
    fn the_live_region_shows_streaming_and_keeps_the_status_visible() {
        let mut state = state_with_input("");
        state.streaming = "Streaming the answer live...".to_string();
        let rendered = render_natural(&state, 60);
        assert!(rendered.contains("assistant: Streaming the answer live..."));
        assert!(rendered.contains("mode:agent"));
    }

    /// The column of the brightest dot in a shimmer row (its grey value peaks at
    /// the crest of the wave).
    fn brightest_col(line: &Line) -> usize {
        line.spans
            .iter()
            .enumerate()
            .max_by_key(|(_, span)| match span.style.fg {
                Some(Color::Rgb(v, _, _)) => v,
                _ => 0,
            })
            .map_or(0, |(i, _)| i)
    }

    #[test]
    fn shimmer_has_the_expected_shape_and_animates() {
        let frame0 = shimmer_lines(0);
        assert_eq!(frame0.len(), SHIMMER_ROWS);
        assert!(frame0.iter().all(|l| l.spans.len() == SHIMMER_WIDTH));

        // The crest moves with the tick, so the brightest column shifts.
        let later = shimmer_lines(6);
        assert_ne!(brightest_col(&frame0[0]), brightest_col(&later[0]));
    }

    #[test]
    fn the_shimmer_placeholder_shows_only_while_waiting_for_the_reply() {
        // The Braille dot-cells are unique to the shimmer, so match on the run.
        let dot_row = "⣿".repeat(SHIMMER_WIDTH);

        let mut state = state_with_input("");
        state.busy = true;
        assert!(
            render_natural(&state, 60).contains(&dot_row),
            "waiting shows the shimmer"
        );

        // Once tokens stream, the real answer replaces the placeholder.
        state.streaming = "the answer".to_string();
        let rendered = render_natural(&state, 60);
        assert!(rendered.contains("assistant: the answer"));
        assert!(!rendered.contains(&dot_row));
    }

    #[test]
    fn an_active_tool_is_shown_as_a_live_indicator() {
        let mut state = state_with_input("");
        state.active_tools.push(ActiveTool {
            id: "1".to_string(),
            name: "run_shell".to_string(),
        });
        assert!(render_natural(&state, 60).contains("run_shell running"));
    }

    #[test]
    fn a_running_background_process_shows_in_the_status_corner() {
        use crate::state::BackgroundProcess;
        let mut state = state_with_input("");
        state.background = vec![BackgroundProcess {
            id: "bg-1".to_string(),
            command: "npm run dev".to_string(),
            alive: true,
        }];
        let rendered = render_natural(&state, 70);
        assert!(rendered.contains("npm run dev"), "{rendered}");
    }

    #[test]
    fn an_exited_background_process_is_not_shown_in_the_corner() {
        use crate::state::BackgroundProcess;
        let mut state = state_with_input("");
        state.background = vec![BackgroundProcess {
            id: "bg-1".to_string(),
            command: "npm run dev".to_string(),
            alive: false,
        }];
        let rendered = render_natural(&state, 70);
        assert!(!rendered.contains("npm run dev"), "{rendered}");
    }

    #[test]
    fn multiple_background_processes_collapse_to_a_count() {
        use crate::state::BackgroundProcess;
        let mut state = state_with_input("");
        state.background = vec![
            BackgroundProcess {
                id: "bg-1".to_string(),
                command: "npm run dev".to_string(),
                alive: true,
            },
            BackgroundProcess {
                id: "bg-2".to_string(),
                command: "bun serve".to_string(),
                alive: true,
            },
        ];
        let rendered = render_natural(&state, 70);
        assert!(rendered.contains("2 background procs"), "{rendered}");
    }

    #[test]
    fn the_status_line_keeps_model_and_session_visible() {
        let mut state = state_with_input("");
        state.header.model = "test-model".into();
        state.header.session_id = "abcd1234ef".into();
        let rendered = render_natural(&state, 70);
        assert!(rendered.contains("test-model"));
        assert!(rendered.contains("session:abcd1234")); // short, 8 chars
    }

    #[test]
    fn the_trust_gate_shows_fully_at_natural_height() {
        let mut state = state_with_input("");
        state.trust = Some(TrustPrompt {
            path: r"D:\repos\demo".to_string(),
        });
        let rendered = render_natural(&state, 70);
        assert!(rendered.contains("Trust this folder?"));
        assert!(rendered.contains(r"D:\repos\demo"));
        // The action line is fully visible — it is not clipped by the viewport.
        assert!(rendered.contains("[y] trust this folder"));
    }

    #[test]
    fn the_approval_prompt_shows_fully_at_natural_height() {
        let mut state = state_with_input("");
        state.approval = Some(ApprovalRequest {
            tool: "run_shell".to_string(),
            target: "rm -rf build".to_string(),
            risk_class: "destructive".to_string(),
        });
        let rendered = render_natural(&state, 70);
        assert!(rendered.contains("Approve tool?"));
        assert!(rendered.contains("rm -rf build"));
        assert!(rendered.contains("[y] approve"));
    }

    #[test]
    fn the_slash_autocomplete_lists_in_region() {
        let mut state = state_with_input("/c");
        state.slash_picker = Some(SlashPicker {
            query: "/c".to_string(),
            items: vec![
                SlashSuggestion {
                    name: "clear".to_string(),
                    description: "Clear the conversation view".to_string(),
                },
                SlashSuggestion {
                    name: "compact".to_string(),
                    description: "Summarize and compact".to_string(),
                },
            ],
            selected: 0,
        });
        let rendered = render_natural(&state, 70);
        assert!(rendered.contains("/clear"));
        assert!(rendered.contains("Clear the conversation view"));
        assert!(rendered.contains("/compact"));
    }

    #[test]
    fn busy_input_keeps_the_cursor_visible() {
        let mut state = state_with_input("next");
        state.input_cursor = state.input.len();
        state.busy = true;
        let height = live_region_height(&state, 80).max(1);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, height)).unwrap();
        terminal.draw(|frame| render(frame, &state)).unwrap();
        assert!(terminal.get_cursor_position().is_ok());
    }
}
