#![forbid(unsafe_code)]
//! Disposable full-screen lab for proving the terminal interaction model.
//!
//! Run with:
//! `cargo run -p localpilot --example interaction_lab --features tui`

#[path = "interaction_lab/model.rs"]
mod model;

use std::cmp::Ordering;
use std::io::{self, Stdout};
use std::panic;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use arboard::Clipboard;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton,
    MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    self, BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use model::{
    scrollbar_geometry, scrollbar_ratio, ContentPoint, Focus, ItemKind, LabState,
    ScrollbarGeometry, Timeline, TimelineView, Viewport, VisualRow,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};
use ratatui::{Frame, Terminal};
use unicode_segmentation::UnicodeSegmentation;

const FRAME_INTERVAL: Duration = Duration::from_millis(33);
const STREAM_INTERVAL: Duration = Duration::from_millis(90);
const MAX_COMPOSER_ROWS: u16 = 6;
const TIMELINE_PREFIX_WIDTH: u16 = 2;
static TERMINAL_MODES_ACTIVE: AtomicBool = AtomicBool::new(false);
static KEYBOARD_FLAGS_PUSHED: AtomicBool = AtomicBool::new(false);

fn main() -> Result<()> {
    install_panic_restore_hook();
    let fault = requested_fault();
    let mut modes = TerminalModes::enter(fault == FaultMode::SetupError)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("initialize interaction-lab terminal")?;
    terminal.clear().context("clear interaction-lab terminal")?;
    match fault {
        FaultMode::AfterEnterError => {
            return Err(anyhow::anyhow!(
                "injected interaction-lab error after terminal entry"
            ));
        }
        FaultMode::Panic => panic!("injected interaction-lab panic after terminal entry"),
        FaultMode::None | FaultMode::SetupError => {}
    }

    let result = run(&mut terminal);
    let _ = terminal.show_cursor();
    drop(terminal);
    modes.restore();
    result
}

struct TerminalModes {
    active: bool,
}

impl TerminalModes {
    fn enter(inject_setup_error: bool) -> Result<Self> {
        terminal::enable_raw_mode().context("enable raw terminal mode")?;
        TERMINAL_MODES_ACTIVE.store(true, AtomicOrdering::Release);
        let mut guard = Self { active: true };
        if inject_setup_error {
            guard.restore();
            return Err(anyhow::anyhow!(
                "injected interaction-lab error during terminal setup"
            ));
        }
        let setup = execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            Hide
        );
        if let Err(error) = setup {
            guard.restore();
            return Err(error).context("enter interaction-lab terminal modes");
        }
        let keyboard_flags = PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES,
        );
        if execute!(io::stdout(), keyboard_flags).is_ok() {
            KEYBOARD_FLAGS_PUSHED.store(true, AtomicOrdering::Release);
        }
        Ok(guard)
    }

    fn restore(&mut self) {
        if !self.active {
            return;
        }
        restore_terminal_modes();
        self.active = false;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FaultMode {
    None,
    SetupError,
    AfterEnterError,
    Panic,
}

fn requested_fault() -> FaultMode {
    match std::env::args().nth(1).as_deref() {
        Some("--fault=setup-error") => FaultMode::SetupError,
        Some("--fault=after-enter-error") => FaultMode::AfterEnterError,
        Some("--fault=panic") => FaultMode::Panic,
        _ => FaultMode::None,
    }
}

impl Drop for TerminalModes {
    fn drop(&mut self) {
        self.restore();
    }
}

fn restore_terminal_modes() {
    if !TERMINAL_MODES_ACTIVE.swap(false, AtomicOrdering::AcqRel) {
        return;
    }
    if KEYBOARD_FLAGS_PUSHED.swap(false, AtomicOrdering::AcqRel) {
        let _ = execute!(
            io::stdout(),
            Show,
            PopKeyboardEnhancementFlags,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
    } else {
        let _ = execute!(
            io::stdout(),
            Show,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
    }
    let _ = terminal::disable_raw_mode();
}

fn install_panic_restore_hook() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        restore_terminal_modes();
        previous(info);
    }));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PointerDrag {
    None,
    Selection,
    Scrollbar { grab_offset: u16 },
}

struct Runtime {
    state: LabState,
    pointer_drag: PointerDrag,
    clipboard_fallback: Option<String>,
}

impl Runtime {
    fn seeded() -> Self {
        Self {
            state: LabState::seeded(),
            pointer_drag: PointerDrag::None,
            clipboard_fallback: None,
        }
    }

    fn copy_selection(&mut self) {
        let Some(text) = self.state.timeline.selected_text() else {
            self.state.copy_status = "nothing selected".to_string();
            return;
        };
        let bytes = text.len();
        match Clipboard::new().and_then(|mut clipboard| {
            clipboard.set_text(text.clone())?;
            clipboard.get_text()
        }) {
            Ok(read_back) if read_back == text => {
                self.clipboard_fallback = None;
                self.state.copy_status = format!("clipboard verified · {bytes} bytes");
            }
            Ok(_) => {
                self.clipboard_fallback = Some(text);
                self.state.copy_status = format!("clipboard mismatch · retained {bytes} bytes");
            }
            Err(_) => {
                self.clipboard_fallback = Some(text);
                self.state.copy_status = format!("clipboard unavailable · retained {bytes} bytes");
            }
        }
    }
}

#[derive(Debug, Clone)]
struct RenderMap {
    timeline_inner: Rect,
    timeline_text_x: u16,
    timeline_text_width: u16,
    timeline_view: TimelineView,
    scrollbar_x: u16,
    scrollbar: ScrollbarGeometry,
    composer_inner: Rect,
    composer_text_width: u16,
    composer_scroll: usize,
}

fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let mut runtime = Runtime::seeded();
    let mut last_stream = Instant::now();

    while !runtime.state.quit {
        let render_map = draw_synchronized(terminal, &runtime.state)?;
        if event::poll(FRAME_INTERVAL).context("poll terminal event")? {
            match event::read().context("read terminal event")? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    handle_key(&mut runtime, key, &render_map);
                }
                Event::Mouse(mouse) => handle_mouse(&mut runtime, mouse, &render_map),
                Event::Paste(text) => {
                    if runtime.state.focus == Focus::Composer {
                        runtime.state.editor.insert(&text);
                    }
                }
                Event::Resize(_, _) | Event::FocusGained | Event::FocusLost | Event::Key(_) => {}
            }
        }
        if last_stream.elapsed() >= STREAM_INTERVAL {
            runtime.state.timeline.append_stream_tick();
            last_stream = Instant::now();
        }
    }
    Ok(())
}

fn draw_synchronized(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &LabState,
) -> Result<RenderMap> {
    execute!(terminal.backend_mut(), BeginSynchronizedUpdate)
        .context("begin synchronized terminal update")?;
    let mut render_map = None;
    let draw_result = terminal
        .draw(|frame| render_map = Some(render(frame, state)))
        .map(|_| ());
    let end_result = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
    draw_result.context("draw interaction lab")?;
    end_result.context("end synchronized terminal update")?;
    render_map.context("interaction-lab render did not produce a hit-test map")
}

fn render(frame: &mut Frame<'_>, state: &LabState) -> RenderMap {
    let area = frame.area();
    if area.width < 30 || area.height < 10 {
        frame.render_widget(
            Paragraph::new("LocalPilot interaction lab\nresize to at least 30 × 10")
                .style(Style::default().fg(Color::Yellow))
                .wrap(Wrap { trim: false }),
            area,
        );
        return RenderMap {
            timeline_inner: Rect::new(0, 0, 0, 0),
            timeline_text_x: 0,
            timeline_text_width: 1,
            timeline_view: state.timeline.view(1, 1),
            scrollbar_x: 0,
            scrollbar: scrollbar_geometry(0, 0, 0, 0),
            composer_inner: Rect::new(0, 0, 0, 0),
            composer_text_width: 1,
            composer_scroll: 0,
        };
    }
    let editor_width = area.width.saturating_sub(4).max(1);
    let editor_rows = state.editor.visual_rows(editor_width).len() as u16;
    let composer_height = editor_rows.clamp(1, MAX_COMPOSER_ROWS).saturating_add(2);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(composer_height),
            Constraint::Length(2),
        ])
        .split(area);

    render_header(frame, layout[0], state);
    let (timeline_inner, timeline_text_x, timeline_text_width, timeline_view, scrollbar) =
        render_timeline(frame, layout[1], state);
    let (composer_inner, composer_text_width, composer_scroll) =
        render_composer(frame, layout[2], state);
    render_footer(frame, layout[3], state, &timeline_view, area);

    RenderMap {
        timeline_inner,
        timeline_text_x,
        timeline_text_width,
        timeline_view,
        scrollbar_x: timeline_inner.right().saturating_sub(1),
        scrollbar,
        composer_inner,
        composer_text_width,
        composer_scroll,
    }
}

fn render_header(frame: &mut Frame<'_>, area: Rect, state: &LabState) {
    let focus = match state.focus {
        Focus::Composer => "CHAT",
        Focus::ReverseSearch { .. } => "HISTORY SEARCH",
        Focus::Completion { .. } => "COMPLETIONS",
    };
    let stream = if state.timeline.streaming {
        "LIVE"
    } else {
        "PAUSED"
    };
    let title = Line::from(vec![
        Span::styled(
            " LocalPilot ",
            Style::default().fg(Color::Black).bg(Color::Cyan),
        ),
        Span::raw("  Chat   Agents   Models  "),
        Span::styled(
            focus,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(stream, Style::default().fg(Color::Green)),
    ]);
    frame.render_widget(
        Paragraph::new(title).block(Block::default().borders(Borders::BOTTOM)),
        area,
    );
}

fn render_timeline(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &LabState,
) -> (Rect, u16, u16, TimelineView, ScrollbarGeometry) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" conversation · application viewport ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Clear, inner);
    let text_width = inner
        .width
        .saturating_sub(1)
        .saturating_sub(TIMELINE_PREFIX_WIDTH)
        .max(1);
    let text_x = inner.x.saturating_add(TIMELINE_PREFIX_WIDTH);
    let view = state.timeline.view(text_width, inner.height);
    let selection = state.timeline.selection;

    for (offset, row) in view.rows.iter().enumerate() {
        let y = inner.y.saturating_add(offset as u16);
        let line = styled_timeline_row(row, selection);
        line.render(
            Rect::new(inner.x, y, inner.width.saturating_sub(1), 1),
            frame.buffer_mut(),
        );
    }

    let geometry = scrollbar_geometry(
        view.total_rows,
        view.viewport_rows,
        view.start,
        inner.height,
    );
    let scrollbar_x = inner.right().saturating_sub(1);
    for row in 0..inner.height {
        let in_thumb = row >= geometry.thumb_top
            && row < geometry.thumb_top.saturating_add(geometry.thumb_height);
        let cell = &mut frame.buffer_mut()[(scrollbar_x, inner.y.saturating_add(row))];
        cell.set_symbol(if in_thumb { "█" } else { "│" });
        cell.set_style(if in_thumb {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        });
    }
    (inner, text_x, text_width, view, geometry)
}

fn styled_timeline_row(row: &VisualRow, selection: Option<model::Selection>) -> Line<'static> {
    let (glyph, color) = match row.kind {
        ItemKind::User => ("› ", Color::Cyan),
        ItemKind::Assistant => ("  ", Color::White),
        ItemKind::Tool => ("◆ ", Color::Magenta),
        ItemKind::Notice => ("! ", Color::Yellow),
    };
    let prefix = if row.start_byte == 0 { glyph } else { "  " };
    let mut spans = vec![Span::styled(prefix, Style::default().fg(color))];
    for (relative, grapheme) in row.text.grapheme_indices(true) {
        let start = row.start_byte + relative;
        let end = start + grapheme.len();
        let selected =
            selection.is_some_and(|value| value.contains_grapheme(row.item_id, start, end));
        let mut style = Style::default().fg(color);
        if selected {
            style = style.fg(Color::Black).bg(Color::LightCyan);
        }
        spans.push(Span::styled(grapheme.to_string(), style));
    }
    Line::from(spans)
}

fn render_composer(frame: &mut Frame<'_>, area: Rect, state: &LabState) -> (Rect, u16, usize) {
    let focus_marker = if state.focus == Focus::Composer {
        "●"
    } else {
        "○"
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(format!(
            " {focus_marker} prompt · Enter send · Alt+Enter newline "
        ));
    let inner = block.inner(area);
    let width = inner.width.max(1);
    let (cursor_row, cursor_column) = state.editor.cursor_row_and_column(width);
    let visible_rows = usize::from(inner.height.max(1));
    let scroll = cursor_row.saturating_add(1).saturating_sub(visible_rows);
    frame.render_widget(
        Paragraph::new(state.editor.text.clone())
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((scroll as u16, 0)),
        area,
    );

    if state.focus == Focus::Composer {
        let cursor_y = inner
            .y
            .saturating_add(cursor_row.saturating_sub(scroll) as u16)
            .min(inner.bottom().saturating_sub(1));
        let cursor_x = inner
            .x
            .saturating_add(cursor_column)
            .min(inner.right().saturating_sub(1));
        frame.set_cursor_position(Position::new(cursor_x, cursor_y));
    }
    (inner, width, scroll)
}

fn render_footer(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &LabState,
    view: &TimelineView,
    terminal_area: Rect,
) {
    let anchor = match state.timeline.viewport {
        Viewport::FollowBottom => "follow-bottom".to_string(),
        Viewport::Held(point) => format!("held {}:{}", point.item_id, point.byte),
    };
    let selection = match state.timeline.selection {
        Some(value) => format!(
            "{}:{}→{}:{}",
            value.anchor.item_id, value.anchor.byte, value.focus.item_id, value.focus.byte
        ),
        None => "none".to_string(),
    };
    let first = Line::from(format!(
        " {anchor} · rows {}..{}/{} · items {} · tick {} · term {}×{} · selection {selection}",
        view.start,
        view.start.saturating_add(view.rows.len()),
        view.total_rows,
        state.timeline.item_count(),
        state.timeline.stream_tick,
        terminal_area.width,
        terminal_area.height,
    ));
    let second = Line::from(format!(
        " PgUp/PgDn + wheel/drag scroll · drag selects · Ctrl+C copies · F2 stream · F3 copy-on-select={} · Ctrl+Q quit · {}",
        state.copy_on_select, state.copy_status
    ));
    frame.render_widget(Paragraph::new(vec![first, second]), area);
}

fn handle_key(runtime: &mut Runtime, key: KeyEvent, map: &RenderMap) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let page = isize::try_from(usize::from(map.timeline_inner.height.max(1))).unwrap_or(isize::MAX);

    match key.code {
        KeyCode::Char('q') if ctrl => runtime.state.quit = true,
        KeyCode::Char('c') if ctrl => runtime.copy_selection(),
        KeyCode::Char('r') if ctrl => {
            runtime.state.focus = Focus::ReverseSearch { selected: 0 };
        }
        KeyCode::F(2) => runtime.state.timeline.streaming = !runtime.state.timeline.streaming,
        KeyCode::F(3) => runtime.state.copy_on_select = !runtime.state.copy_on_select,
        KeyCode::PageUp => runtime.state.timeline.scroll_by(
            -page,
            map.timeline_text_width,
            map.timeline_inner.height,
        ),
        KeyCode::PageDown => runtime.state.timeline.scroll_by(
            page,
            map.timeline_text_width,
            map.timeline_inner.height,
        ),
        KeyCode::Up => runtime
            .state
            .vertical(Ordering::Less, map.composer_text_width),
        KeyCode::Down => runtime
            .state
            .vertical(Ordering::Greater, map.composer_text_width),
        KeyCode::Tab => runtime.state.focus = Focus::Completion { selected: 0 },
        KeyCode::Esc => match runtime.state.focus {
            Focus::Composer => {
                runtime.state.timeline.streaming = false;
                runtime.state.timeline.clear_selection();
            }
            Focus::ReverseSearch { .. } | Focus::Completion { .. } => {
                runtime.state.focus = Focus::Composer;
            }
        },
        KeyCode::Enter if alt || shift => runtime.state.editor.insert("\n"),
        KeyCode::Char('j') if ctrl => runtime.state.editor.insert("\n"),
        KeyCode::Enter => match runtime.state.focus {
            Focus::Composer => runtime.state.submit(),
            Focus::ReverseSearch { .. } | Focus::Completion { .. } => {
                runtime.state.focus = Focus::Composer;
            }
        },
        KeyCode::Left if runtime.state.focus == Focus::Composer => runtime.state.editor.move_left(),
        KeyCode::Right if runtime.state.focus == Focus::Composer => {
            runtime.state.editor.move_right();
        }
        KeyCode::Backspace if runtime.state.focus == Focus::Composer => {
            runtime.state.editor.backspace();
        }
        KeyCode::Char(character) if runtime.state.focus == Focus::Composer && !ctrl && !alt => {
            runtime.state.editor.insert(&character.to_string());
        }
        _ => {}
    }
}

fn handle_mouse(runtime: &mut Runtime, mouse: MouseEvent, map: &RenderMap) {
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            runtime
                .state
                .timeline
                .scroll_by(-3, map.timeline_text_width, map.timeline_inner.height)
        }
        MouseEventKind::ScrollDown => {
            runtime
                .state
                .timeline
                .scroll_by(3, map.timeline_text_width, map.timeline_inner.height)
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if mouse.column == map.scrollbar_x && row_in(mouse.row, map.timeline_inner) {
                let local_row = mouse.row.saturating_sub(map.timeline_inner.y);
                if local_row >= map.scrollbar.thumb_top
                    && local_row
                        < map
                            .scrollbar
                            .thumb_top
                            .saturating_add(map.scrollbar.thumb_height)
                {
                    runtime.pointer_drag = PointerDrag::Scrollbar {
                        grab_offset: local_row.saturating_sub(map.scrollbar.thumb_top),
                    };
                } else {
                    jump_scrollbar(runtime, local_row, map, 0);
                    runtime.pointer_drag = PointerDrag::Scrollbar { grab_offset: 0 };
                }
            } else if point_in(mouse.column, mouse.row, map.timeline_inner) {
                if let Some(point) = timeline_point(mouse.column, mouse.row, map, false) {
                    runtime.state.timeline.start_selection(point);
                    runtime.pointer_drag = PointerDrag::Selection;
                }
            } else if point_in(mouse.column, mouse.row, map.composer_inner) {
                let visual_row = map
                    .composer_scroll
                    .saturating_add(usize::from(mouse.row.saturating_sub(map.composer_inner.y)));
                let column = mouse.column.saturating_sub(map.composer_inner.x);
                runtime.state.editor.set_cursor_from_visual(
                    visual_row,
                    column,
                    map.composer_text_width,
                );
                runtime.state.focus = Focus::Composer;
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => match runtime.pointer_drag {
            PointerDrag::Selection => extend_mouse_selection(runtime, mouse, map),
            PointerDrag::Scrollbar { grab_offset } => {
                let local_row = mouse
                    .row
                    .saturating_sub(map.timeline_inner.y)
                    .min(map.timeline_inner.height.saturating_sub(1));
                jump_scrollbar(runtime, local_row, map, grab_offset);
            }
            PointerDrag::None => {}
        },
        MouseEventKind::Up(MouseButton::Left) => {
            if runtime.pointer_drag == PointerDrag::Selection && runtime.state.copy_on_select {
                runtime.copy_selection();
            }
            runtime.pointer_drag = PointerDrag::None;
        }
        MouseEventKind::Moved
        | MouseEventKind::Down(_)
        | MouseEventKind::Up(_)
        | MouseEventKind::Drag(_)
        | MouseEventKind::ScrollLeft
        | MouseEventKind::ScrollRight => {}
    }
}

fn jump_scrollbar(runtime: &mut Runtime, local_row: u16, map: &RenderMap, grab_offset: u16) {
    let travel = map
        .timeline_inner
        .height
        .saturating_sub(map.scrollbar.thumb_height);
    let thumb_top = local_row.saturating_sub(grab_offset).min(travel);
    let ratio = scrollbar_ratio(thumb_top, travel.saturating_add(1));
    runtime
        .state
        .timeline
        .jump_to_ratio(ratio, map.timeline_text_width, map.timeline_inner.height);
}

fn extend_mouse_selection(runtime: &mut Runtime, mouse: MouseEvent, map: &RenderMap) {
    let visual_index = if mouse.row < map.timeline_inner.y {
        runtime
            .state
            .timeline
            .scroll_by(-1, map.timeline_text_width, map.timeline_inner.height);
        0
    } else if mouse.row >= map.timeline_inner.bottom() {
        runtime
            .state
            .timeline
            .scroll_by(1, map.timeline_text_width, map.timeline_inner.height);
        usize::from(map.timeline_inner.height.saturating_sub(1))
    } else {
        usize::from(mouse.row.saturating_sub(map.timeline_inner.y))
    };
    let fresh_view = runtime
        .state
        .timeline
        .view(map.timeline_text_width, map.timeline_inner.height);
    if let Some(point) = timeline_point_in_view(mouse.column, visual_index, &fresh_view, map, true)
    {
        runtime.state.timeline.extend_selection(point);
    }
}

fn timeline_point(column: u16, row: u16, map: &RenderMap, trailing: bool) -> Option<ContentPoint> {
    let visual_index = usize::from(row.saturating_sub(map.timeline_inner.y));
    timeline_point_in_view(column, visual_index, &map.timeline_view, map, trailing)
}

fn timeline_point_in_view(
    column: u16,
    visual_index: usize,
    view: &TimelineView,
    map: &RenderMap,
    trailing: bool,
) -> Option<ContentPoint> {
    let visual_row = view.rows.get(visual_index)?;
    let text_column = column
        .saturating_sub(map.timeline_text_x)
        .min(map.timeline_text_width);
    Some(Timeline::point_for_column(
        visual_row,
        text_column,
        trailing,
    ))
}

fn point_in(column: u16, row: u16, area: Rect) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

fn row_in(row: u16, area: Rect) -> bool {
    row >= area.y && row < area.bottom()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_testing_excludes_right_and_bottom_edges() {
        let area = Rect::new(3, 4, 10, 5);
        assert!(point_in(3, 4, area));
        assert!(point_in(12, 8, area));
        assert!(!point_in(13, 8, area));
        assert!(!point_in(12, 9, area));
    }
}
