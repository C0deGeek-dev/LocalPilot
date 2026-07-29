use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};
use ratatui::Frame;

use crate::{AppModel, ColorSupport, Focus, ItemKind, Selection, VisualRow, APP_NAME};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HitMap {
    pub timeline: Rect,
    pub composer: Rect,
    pub editor_width: u16,
    pub composer_scroll: usize,
}

#[must_use]
pub fn render(frame: &mut Frame<'_>, app: &AppModel) -> HitMap {
    let area = frame.area();
    if area.width < 30 || area.height < 10 {
        frame.render_widget(
            Paragraph::new(format!("{APP_NAME}\nresize to at least 30 × 10"))
                .wrap(Wrap { trim: false }),
            area,
        );
        return HitMap {
            timeline: Rect::default(),
            composer: Rect::default(),
            editor_width: 1,
            composer_scroll: 0,
        };
    }

    let editor_width = area.width.saturating_sub(4).max(1);
    let editor_rows = u16::try_from(app.editor.visual_rows(editor_width).len()).unwrap_or(u16::MAX);
    let composer_height = editor_rows.clamp(1, 6).saturating_add(2);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(composer_height),
            Constraint::Length(1),
        ])
        .split(area);

    render_header(frame, layout[0], app);
    let timeline = render_timeline(frame, layout[1], app);
    let (composer, editor_width, composer_scroll) = render_composer(frame, layout[2], app);
    render_footer(frame, layout[3], app);
    HitMap {
        timeline,
        composer,
        editor_width,
        composer_scroll,
    }
}

fn render_header(frame: &mut Frame<'_>, area: Rect, app: &AppModel) {
    let accent = color(app, Color::Cyan);
    let line = Line::from(vec![
        Span::styled(
            format!(" {APP_NAME} "),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  Chat"),
        Span::styled(
            format!("  {} · {}", app.header.workspace, app.header.model),
            Style::default().fg(color(app, Color::DarkGray)),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(line).block(Block::default().borders(Borders::BOTTOM)),
        area,
    );
}

fn render_timeline(frame: &mut Frame<'_>, area: Rect, app: &AppModel) -> Rect {
    let inner = Rect::new(
        area.x.saturating_add(1),
        area.y,
        area.width.saturating_sub(2),
        area.height,
    );
    let view = app.timeline.view(inner.width.max(1), inner.height.max(1));
    for (offset, row) in view.rows.iter().enumerate() {
        let y = inner.y.saturating_add(offset as u16);
        timeline_line(row, app.timeline.selection, app)
            .render(Rect::new(inner.x, y, inner.width, 1), frame.buffer_mut());
    }
    inner
}

fn timeline_line(row: &VisualRow, selection: Option<Selection>, app: &AppModel) -> Line<'static> {
    use unicode_segmentation::UnicodeSegmentation;

    let item_color = match row.kind {
        ItemKind::User => Color::Cyan,
        ItemKind::Assistant => Color::White,
        ItemKind::Reasoning => Color::DarkGray,
        ItemKind::Tool => Color::Magenta,
        ItemKind::Notice => Color::Yellow,
    };
    let mut spans = Vec::new();
    for (relative, grapheme) in row.text.grapheme_indices(true) {
        let start = row.start_byte + relative;
        let end = start + grapheme.len();
        let selected =
            selection.is_some_and(|value| value.contains_grapheme(row.item_id, start, end));
        let mut style = Style::default().fg(color(app, item_color));
        if selected {
            style = if app.capabilities.color == ColorSupport::Color {
                style.fg(Color::Black).bg(Color::LightCyan)
            } else {
                style.add_modifier(Modifier::REVERSED)
            };
        }
        spans.push(Span::styled(grapheme.to_string(), style));
    }
    Line::from(spans)
}

fn render_composer(frame: &mut Frame<'_>, area: Rect, app: &AppModel) -> (Rect, u16, usize) {
    let border = if app.focus == Focus::Composer {
        color(app, Color::Cyan)
    } else {
        color(app, Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border));
    let inner = block.inner(area);
    let width = inner.width.max(1);
    let (cursor_row, cursor_column) = app.editor.cursor_row_and_column(width);
    let visible_rows = usize::from(inner.height.max(1));
    let scroll = cursor_row.saturating_add(1).saturating_sub(visible_rows);
    frame.render_widget(
        Paragraph::new(app.editor.text().to_string())
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
        area,
    );
    if app.focus == Focus::Composer {
        let cursor_y = inner
            .y
            .saturating_add(u16::try_from(cursor_row.saturating_sub(scroll)).unwrap_or(u16::MAX))
            .min(inner.bottom().saturating_sub(1));
        let cursor_x = inner
            .x
            .saturating_add(cursor_column)
            .min(inner.right().saturating_sub(1));
        frame.set_cursor_position(Position::new(cursor_x, cursor_y));
    }
    (inner, width, scroll)
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &AppModel) {
    let state = match (app.work, app.exit_armed) {
        (_, true) => "press Ctrl+C again to exit",
        (crate::WorkState::Idle, false) => "idle · Ctrl+C copy / twice to exit",
        (
            crate::WorkState::Busy {
                cancellation_requested: false,
            },
            false,
        ) => "working · Ctrl+C cancel / twice to exit",
        (
            crate::WorkState::Busy {
                cancellation_requested: true,
            },
            false,
        ) => "cancelling · Ctrl+C again to exit",
    };
    frame.render_widget(
        Paragraph::new(format!(" {state}")).style(Style::default().fg(color(app, Color::DarkGray))),
        area,
    );
}

fn color(app: &AppModel, requested: Color) -> Color {
    if app.capabilities.color == ColorSupport::Color {
        requested
    } else {
        Color::Reset
    }
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::*;
    use crate::{Header, TerminalCapabilities};

    #[test]
    fn idle_skeleton_renders_to_a_backend_neutral_frame() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let app = AppModel::new(
            Header {
                version: "0".to_string(),
                provider: "provider".to_string(),
                model: "model".to_string(),
                workspace: "workspace".to_string(),
                session_id: "session".to_string(),
                session_name: None,
            },
            TerminalCapabilities::default(),
        );
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw skeleton");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains(APP_NAME));
        assert!(rendered.contains("workspace · model"));
        let hit_map = hit_map.expect("hit map");
        assert!(hit_map.timeline.height > 0);
        assert!(hit_map.composer.height > 0);
    }
}
