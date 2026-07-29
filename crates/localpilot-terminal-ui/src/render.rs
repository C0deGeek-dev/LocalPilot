use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};
use ratatui::Frame;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::{
    ActivityState, AppModel, Focus, FrameLayout, ItemKind, PinnedPrompt, Selection, TabId,
    TextStyle, ThemeResolver, UiRole, VisualRow, VisualRowPart, APP_NAME, MINIMUM_HEIGHT,
    MINIMUM_WIDTH,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabHit {
    pub tab: TabId,
    pub area: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarGeometry {
    pub track: Rect,
    pub thumb: Option<Rect>,
    pub start: usize,
    pub total_rows: usize,
    pub viewport_rows: usize,
}

impl ScrollbarGeometry {
    #[must_use]
    pub fn calculate(track: Rect, start: usize, total_rows: usize, viewport_rows: usize) -> Self {
        let thumb = if track.height == 0 || total_rows <= viewport_rows {
            None
        } else {
            let track_height = usize::from(track.height);
            let thumb_height = viewport_rows
                .saturating_mul(track_height)
                .div_ceil(total_rows)
                .clamp(1, track_height);
            let max_thumb_start = track_height.saturating_sub(thumb_height);
            let max_view_start = total_rows.saturating_sub(viewport_rows);
            let thumb_start = if max_view_start == 0 {
                0
            } else {
                start
                    .saturating_mul(max_thumb_start)
                    .saturating_add(max_view_start / 2)
                    / max_view_start
            };
            Some(Rect::new(
                track.x,
                track
                    .y
                    .saturating_add(u16::try_from(thumb_start).unwrap_or(u16::MAX)),
                track.width,
                u16::try_from(thumb_height).unwrap_or(track.height),
            ))
        };
        Self {
            track,
            thumb,
            start,
            total_rows,
            viewport_rows,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitMap {
    pub frame: Option<FrameLayout>,
    pub tabs: Vec<TabHit>,
    pub timeline: Rect,
    pub scrollbar: ScrollbarGeometry,
    pub composer: Rect,
    pub editor_width: u16,
    pub composer_scroll: usize,
}

#[must_use]
pub fn render(frame: &mut Frame<'_>, app: &AppModel) -> HitMap {
    let area = frame.area();
    let prospective_editor_width = area.width.saturating_sub(2).max(1);
    let requested_editor_rows = u16::try_from(
        app.editor
            .visual_rows(prospective_editor_width)
            .len()
            .max(1),
    )
    .unwrap_or(u16::MAX);
    let Some(layout) = FrameLayout::calculate(area, requested_editor_rows) else {
        frame.render_widget(
            Paragraph::new(format!(
                "{APP_NAME}\nresize to at least {MINIMUM_WIDTH} × {MINIMUM_HEIGHT}"
            ))
            .wrap(Wrap { trim: false }),
            area,
        );
        return HitMap {
            frame: None,
            tabs: Vec::new(),
            timeline: Rect::default(),
            scrollbar: ScrollbarGeometry::calculate(Rect::default(), 0, 0, 0),
            composer: Rect::default(),
            editor_width: 1,
            composer_scroll: 0,
        };
    };

    let tabs = render_tabs(frame, layout.tabs, app);
    let scrollbar = render_timeline(frame, layout, app);
    render_status(frame, layout.status, app, layout.stacked);
    let (editor_width, composer_scroll) = render_composer(frame, layout, app);
    render_footer(frame, layout.footer, app, layout.stacked);
    HitMap {
        frame: Some(layout),
        tabs,
        timeline: layout.timeline_content,
        scrollbar,
        composer: layout.composer_content,
        editor_width,
        composer_scroll,
    }
}

fn render_tabs(frame: &mut Frame<'_>, area: Rect, app: &AppModel) -> Vec<TabHit> {
    let theme = theme(app);
    let mut hits = Vec::new();
    let mut x = area.x;
    for tab in &app.tabs {
        let label = format!("  {}  ", tab.label());
        let width = u16::try_from(UnicodeWidthStr::width(label.as_str())).unwrap_or(u16::MAX);
        if width > area.right().saturating_sub(x) {
            let arrow = Rect::new(area.right().saturating_sub(1), area.y, 1, 1);
            frame.render_widget(
                Paragraph::new("›").style(theme.ui(UiRole::TabInactive)),
                arrow,
            );
            break;
        }
        let tab_area = Rect::new(x, area.y, width, 1);
        let role = if *tab == app.active_tab {
            UiRole::TabActive
        } else {
            UiRole::TabInactive
        };
        frame.render_widget(Paragraph::new(label).style(theme.ui(role)), tab_area);
        hits.push(TabHit {
            tab: *tab,
            area: tab_area,
        });
        x = x.saturating_add(width);
    }
    hits
}

fn render_timeline(
    frame: &mut Frame<'_>,
    layout: FrameLayout,
    app: &AppModel,
) -> ScrollbarGeometry {
    let area = layout.timeline_content;
    let view = app.timeline.view(area.width.max(1), area.height.max(1));
    let content_offset = if let Some(pinned) = &view.pinned {
        render_pinned_prompt(frame, area, pinned, app);
        1
    } else {
        0
    };
    if view.rows.is_empty() && view.pinned.is_none() {
        let theme = theme(app);
        let banner = Line::from(vec![
            Span::styled("● ", theme.ui(UiRole::Accent)),
            Span::styled(
                APP_NAME,
                theme.text(TextStyle::new(crate::SemanticRole::Heading)),
            ),
            Span::styled(
                format!(" · {}", app.header.provider),
                theme.ui(UiRole::Muted),
            ),
        ]);
        banner.render(Rect::new(area.x, area.y, area.width, 1), frame.buffer_mut());
    } else {
        for (offset, row) in view.rows.iter().enumerate() {
            let y = area
                .y
                .saturating_add(content_offset)
                .saturating_add(offset as u16);
            timeline_line(row, app.timeline.selection, app, area.width)
                .render(Rect::new(area.x, y, area.width, 1), frame.buffer_mut());
        }
    }

    let scrollbar = ScrollbarGeometry::calculate(
        layout.scrollbar,
        view.start,
        view.total_rows,
        view.viewport_rows,
    );
    if let Some(thumb) = scrollbar.thumb {
        let theme = theme(app);
        for y in scrollbar.track.y..scrollbar.track.bottom() {
            frame.render_widget(
                Paragraph::new("│").style(theme.ui(UiRole::Border)),
                Rect::new(scrollbar.track.x, y, 1, 1),
            );
        }
        for y in thumb.y..thumb.bottom() {
            frame.render_widget(
                Paragraph::new("█").style(theme.ui(UiRole::Accent)),
                Rect::new(thumb.x, y, 1, 1),
            );
        }
    }
    scrollbar
}

fn render_pinned_prompt(frame: &mut Frame<'_>, area: Rect, pin: &PinnedPrompt, app: &AppModel) {
    let theme = theme(app);
    let glyph = if pin.overflowing { "↓ " } else { "❯ " };
    let trailing = pin.trailing.as_deref().unwrap_or("");
    let fixed_width = UnicodeWidthStr::width(glyph)
        .saturating_add(UnicodeWidthStr::width(trailing))
        .saturating_add(2);
    let text_budget = usize::from(area.width).saturating_sub(fixed_width);
    let text = truncate_end(&pin.text, u16::try_from(text_budget).unwrap_or(u16::MAX));
    let used = fixed_width.saturating_add(UnicodeWidthStr::width(text.as_str()));
    let gap = usize::from(area.width).saturating_sub(used);
    let line = Line::from(vec![
        Span::styled(glyph, theme.ui(UiRole::Accent)),
        Span::styled(text, theme.text(TextStyle::new(crate::SemanticRole::User))),
        Span::raw(" ".repeat(gap)),
        Span::styled(trailing.to_string(), theme.ui(UiRole::Muted)),
        Span::styled(" ┃", theme.ui(UiRole::Accent)),
    ]);
    line.render(Rect::new(area.x, area.y, area.width, 1), frame.buffer_mut());
}

fn timeline_line(
    row: &VisualRow,
    selection: Option<Selection>,
    app: &AppModel,
    width: u16,
) -> Line<'static> {
    let theme = theme(app);
    if row.part == VisualRowPart::FrameTop {
        return framed_rule(width, true, theme.ui(UiRole::Accent));
    }
    if row.part == VisualRowPart::FrameBottom {
        return framed_rule(width, false, theme.ui(UiRole::Accent));
    }

    let VisualRowPart::Content { first, .. } = row.part else {
        return Line::default();
    };
    let mut spans = role_prefix(row.kind, row.activity, first, theme);
    let prefix_width = spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>();
    let mut content_width = 0usize;
    let mut rendered_content = false;
    for visual_span in &row.spans {
        let relative_start = visual_span.start_byte.saturating_sub(row.start_byte);
        let relative_end = visual_span.end_byte.saturating_sub(row.start_byte);
        for (offset, grapheme) in row.text[relative_start..relative_end].grapheme_indices(true) {
            let start = visual_span.start_byte + offset;
            let end = start + grapheme.len();
            let selected =
                selection.is_some_and(|value| value.contains_grapheme(row.item_id, start, end));
            let mut style = theme.text(visual_span.style);
            if selected {
                style = theme.selected(style);
            }
            content_width = content_width.saturating_add(UnicodeWidthStr::width(grapheme));
            rendered_content = true;
            spans.push(Span::styled(grapheme.to_string(), style));
        }
    }
    if !rendered_content && !row.text.is_empty() {
        let fallback = TextStyle::new(row.kind.into());
        content_width = UnicodeWidthStr::width(row.text.as_str());
        spans.push(Span::styled(row.text.clone(), theme.text(fallback)));
    }
    if row.kind == ItemKind::User {
        let trailing = row.trailing.as_deref().unwrap_or("");
        let trailing_width = UnicodeWidthStr::width(trailing);
        let reserved = prefix_width
            .saturating_add(content_width)
            .saturating_add(trailing_width)
            .saturating_add(1);
        let gap = usize::from(width).saturating_sub(reserved);
        if gap > 0 {
            spans.push(Span::raw(" ".repeat(gap)));
        }
        if !trailing.is_empty() {
            spans.push(Span::styled(trailing.to_string(), theme.ui(UiRole::Muted)));
        }
        spans.push(Span::styled("┃", theme.ui(UiRole::Accent)));
    }
    Line::from(spans)
}

fn framed_rule(width: u16, top: bool, style: ratatui::style::Style) -> Line<'static> {
    let (corner, fill) = if top { ("╻", "▄") } else { ("╹", "▀") };
    let middle = fill.repeat(usize::from(width.saturating_sub(2)));
    Line::styled(format!("{corner}{middle}{corner}"), style)
}

fn role_prefix(
    kind: ItemKind,
    activity: Option<ActivityState>,
    first: bool,
    theme: ThemeResolver,
) -> Vec<Span<'static>> {
    match kind {
        ItemKind::User => vec![
            Span::styled("┃ ", theme.ui(UiRole::Accent)),
            Span::styled(if first { "❯ " } else { "  " }, theme.ui(UiRole::Accent)),
        ],
        ItemKind::Assistant => vec![Span::styled(
            if first { "● " } else { "  " },
            theme.ui(UiRole::Accent),
        )],
        ItemKind::Reasoning => vec![Span::styled(
            if first { "◌ " } else { "  " },
            theme.ui(UiRole::Muted),
        )],
        ItemKind::Tool => {
            let (glyph, role) = match activity {
                Some(ActivityState::Running) | None => ("◉ ", UiRole::Code),
                Some(ActivityState::Success) => ("✓ ", UiRole::Success),
                Some(ActivityState::Error) => ("× ", UiRole::Error),
            };
            vec![Span::styled(
                if first { glyph } else { "  " },
                theme.ui(role),
            )]
        }
        ItemKind::Notice => vec![Span::styled(
            if first { "! " } else { "  " },
            theme.ui(UiRole::Warning),
        )],
    }
}

fn render_status(frame: &mut Frame<'_>, area: Rect, app: &AppModel, narrow: bool) {
    let theme = theme(app);
    let right = status_right(app);
    if narrow {
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    middle_elide(&app.header.workspace, area.width),
                    theme.ui(UiRole::Foreground),
                ),
                Line::styled(truncate_end(&right, area.width), theme.ui(UiRole::Muted)),
            ]),
            area,
        );
    } else {
        frame.render_widget(
            Paragraph::new(two_sided(&app.header.workspace, &right, area.width))
                .style(theme.ui(UiRole::Muted)),
            area,
        );
    }
}

fn status_right(app: &AppModel) -> String {
    let mut parts = vec![app.header.model.clone()];
    if let Some((input, output)) = app.usage {
        parts.push(format!("{} tokens", input.saturating_add(output)));
    }
    if let Some((used, limit)) = app.context_usage {
        let percentage = if limit == 0 {
            0
        } else {
            used.saturating_mul(100) / limit
        };
        parts.push(format!("{percentage}% context"));
    }
    parts.join(" · ")
}

fn render_composer(frame: &mut Frame<'_>, layout: FrameLayout, app: &AppModel) -> (u16, usize) {
    let theme = theme(app);
    let border = if app.focus == Focus::Composer {
        theme.ui(UiRole::Focus)
    } else {
        theme.ui(UiRole::Border)
    };
    let inner = layout.composer_content;
    let width = inner.width.max(1);
    let (cursor_row, cursor_column) = app.editor.cursor_row_and_column(width);
    let visible_rows = usize::from(inner.height.max(1));
    let scroll = cursor_row.saturating_add(1).saturating_sub(visible_rows);
    render_slim_frame(frame, layout.composer, border);
    frame.render_widget(
        Paragraph::new(app.editor.text().to_string())
            .wrap(Wrap { trim: false })
            .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
        inner,
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
    (width, scroll)
}

fn render_slim_frame(frame: &mut Frame<'_>, area: Rect, style: ratatui::style::Style) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    framed_rule(area.width, true, style)
        .render(Rect::new(area.x, area.y, area.width, 1), frame.buffer_mut());
    framed_rule(area.width, false, style).render(
        Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
        frame.buffer_mut(),
    );
    for y in area.y.saturating_add(1)..area.bottom().saturating_sub(1) {
        frame.render_widget(Paragraph::new("┃").style(style), Rect::new(area.x, y, 1, 1));
        frame.render_widget(
            Paragraph::new("┃").style(style),
            Rect::new(area.right().saturating_sub(1), y, 1, 1),
        );
    }
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &AppModel, narrow: bool) {
    let state = footer_state(app);
    let shortcuts = "? help · / commands";
    let theme = theme(app);
    let text = if narrow {
        format!(
            "{}\n{}",
            truncate_end(state, area.width),
            truncate_end(shortcuts, area.width)
        )
    } else {
        two_sided(shortcuts, state, area.width)
    };
    frame.render_widget(Paragraph::new(text).style(theme.ui(UiRole::Muted)), area);
}

fn footer_state(app: &AppModel) -> &'static str {
    match (app.work, app.exit_armed) {
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
    }
}

fn two_sided(left: &str, right: &str, width: u16) -> String {
    let width = usize::from(width);
    if width == 0 {
        return String::new();
    }
    let right = truncate_end(right, u16::try_from(width).unwrap_or(u16::MAX));
    let right_width = UnicodeWidthStr::width(right.as_str());
    if right_width >= width {
        return right;
    }
    let left_budget = width.saturating_sub(right_width).saturating_sub(1);
    let left = middle_elide(left, u16::try_from(left_budget).unwrap_or(u16::MAX));
    let left_width = UnicodeWidthStr::width(left.as_str());
    let gap = width.saturating_sub(left_width).saturating_sub(right_width);
    format!("{left}{}{right}", " ".repeat(gap))
}

fn truncate_end(text: &str, width: u16) -> String {
    truncate_parts(text, width, false)
}

fn middle_elide(text: &str, width: u16) -> String {
    truncate_parts(text, width, true)
}

fn truncate_parts(text: &str, width: u16, middle: bool) -> String {
    let width = usize::from(width);
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }
    let graphemes: Vec<&str> = text.graphemes(true).collect();
    if !middle {
        let mut output = String::new();
        for grapheme in graphemes {
            if UnicodeWidthStr::width(output.as_str())
                .saturating_add(UnicodeWidthStr::width(grapheme))
                >= width
            {
                break;
            }
            output.push_str(grapheme);
        }
        output.push('…');
        return output;
    }

    let budget = width - 1;
    let mut left = String::new();
    let mut right = String::new();
    let mut left_index = 0usize;
    let mut right_index = graphemes.len();
    while left_index < right_index {
        let candidate = graphemes[left_index];
        if UnicodeWidthStr::width(left.as_str())
            .saturating_add(UnicodeWidthStr::width(right.as_str()))
            .saturating_add(UnicodeWidthStr::width(candidate))
            > budget
        {
            break;
        }
        left.push_str(candidate);
        left_index += 1;
        if left_index >= right_index {
            break;
        }
        let candidate = graphemes[right_index - 1];
        if UnicodeWidthStr::width(left.as_str())
            .saturating_add(UnicodeWidthStr::width(right.as_str()))
            .saturating_add(UnicodeWidthStr::width(candidate))
            > budget
        {
            break;
        }
        right.insert_str(0, candidate);
        right_index -= 1;
    }
    format!("{left}…{right}")
}

fn theme(app: &AppModel) -> ThemeResolver {
    ThemeResolver::new(app.theme, app.capabilities.color)
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    use super::*;
    use crate::{ColorSupport, Header, ItemKind, TerminalCapabilities, Theme};

    fn model() -> AppModel {
        AppModel::new(
            Header {
                version: "0".to_string(),
                provider: "provider".to_string(),
                model: "model".to_string(),
                workspace: "workspace".to_string(),
                session_id: "session".to_string(),
                session_name: None,
            },
            TerminalCapabilities::default(),
        )
    }

    #[test]
    fn idle_shell_renders_target_regions_to_a_backend_neutral_frame() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let app = model();
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw shell");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains(APP_NAME));
        assert!(rendered.contains("workspace"));
        assert!(rendered.contains("model"));
        let hit_map = hit_map.expect("hit map");
        assert_eq!(hit_map.tabs.len(), 1);
        assert!(hit_map.timeline.height > 0);
        assert!(hit_map.composer.height > 0);
        assert_eq!(
            hit_map.timeline.right(),
            hit_map.scrollbar.track.x,
            "timeline wrapping width must always exclude the scrollbar gutter"
        );
    }

    #[test]
    fn overflowing_timeline_draws_and_reports_a_proportional_scrollbar() {
        let backend = TestBackend::new(40, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = model();
        for number in 0..100 {
            let _ = app
                .timeline
                .push(ItemKind::Assistant, format!("response {number:03}"));
        }
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw overflowing shell");
        let hit_map = hit_map.expect("hit map");
        let thumb = hit_map.scrollbar.thumb.expect("scrollbar thumb");
        assert!(thumb.height >= 1);
        assert_eq!(thumb.bottom(), hit_map.scrollbar.track.bottom());
        assert!(terminal.backend().to_string().contains('█'));
    }

    #[test]
    fn narrow_tabs_hide_whole_overflowing_tabs_behind_an_arrow() {
        let backend = TestBackend::new(30, 10);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = model();
        app.set_tabs([
            TabId::Session,
            TabId::Activity,
            TabId::Settings,
            TabId::Plan,
        ]);
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw narrow tabs");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains('›'));
        assert!(hit_map.expect("hit map").tabs.len() < app.tabs.len());
    }

    #[test]
    fn display_width_elision_never_exceeds_its_budget() {
        for width in 0..12 {
            assert!(
                UnicodeWidthStr::width(middle_elide("a/界/very/long/path", width).as_str())
                    <= usize::from(width)
            );
            assert!(
                UnicodeWidthStr::width(truncate_end("emoji 🧪 tail", width).as_str())
                    <= usize::from(width)
            );
        }
    }

    #[test]
    fn scrollbar_geometry_is_hidden_when_content_fits() {
        let geometry = ScrollbarGeometry::calculate(Rect::new(79, 1, 1, 10), 0, 10, 10);
        assert_eq!(geometry.thumb, None);
        let geometry = ScrollbarGeometry::calculate(Rect::new(79, 1, 1, 10), 90, 100, 10);
        assert_eq!(geometry.thumb.expect("thumb").y, 10);
    }

    #[test]
    fn style_type_is_kept_out_of_public_state() {
        let _: ratatui::style::Style = theme(&model()).ui(UiRole::Foreground);
    }

    fn buffer_line(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .filter_map(|x| buffer.cell((x, y)))
            .map(ratatui::buffer::Cell::symbol)
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn observable_frame_goldens_hold_at_wide_standard_and_narrow_sizes() {
        for (width, height) in [(120, 30), (80, 24), (40, 20)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            let mut app = model();
            app.set_tabs([
                TabId::Session,
                TabId::Plan,
                TabId::Activity,
                TabId::Settings,
            ]);
            app.editor.insert("draft");
            for number in 0..80 {
                let _ = app
                    .timeline
                    .push(ItemKind::Assistant, format!("response {number:03}"));
            }
            let mut hit_map = None;
            terminal
                .draw(|frame| hit_map = Some(render(frame, &app)))
                .expect("draw frame golden");
            let hit_map = hit_map.expect("hit map");
            let layout = hit_map.frame.expect("frame layout");
            let buffer = terminal.backend().buffer();

            assert!(buffer_line(buffer, 0).contains("Session"));
            assert_eq!(buffer[(0, layout.composer.y)].symbol(), "╻");
            assert_eq!(buffer[(0, layout.composer.bottom() - 1)].symbol(), "╹");
            assert_eq!(
                buffer[(layout.scrollbar.x, layout.scrollbar.y)].symbol(),
                "│"
            );
            assert!(buffer_line(buffer, layout.status.y).contains("workspace"));
            assert!(buffer_line(buffer, layout.footer.y).contains("Ctrl+C"));
            if width == 40 {
                assert!(buffer_line(buffer, 0).contains('›'));
                assert_eq!(layout.status.height, 2);
                assert_eq!(layout.footer.height, 2);
            } else {
                assert_eq!(layout.status.height, 1);
                assert_eq!(layout.footer.height, 1);
            }
        }
    }

    #[test]
    fn theme_frame_goldens_route_active_tabs_through_the_resolver() {
        for theme_name in [
            Theme::Default,
            Theme::Dim,
            Theme::HighContrast,
            Theme::Colorblind,
        ] {
            let backend = TestBackend::new(80, 24);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            let mut app = model();
            app.theme = theme_name;
            terminal
                .draw(|frame| {
                    let _ = render(frame, &app);
                })
                .expect("draw themed frame");
            let actual = terminal.backend().buffer()[(0, 0)].style();
            let expected =
                ThemeResolver::new(theme_name, ColorSupport::Color).ui(UiRole::TabActive);
            assert_eq!(actual.fg, expected.fg);
            assert_eq!(actual.bg, expected.bg);
            assert!(actual.add_modifier.contains(expected.add_modifier));
        }
    }

    #[test]
    fn no_color_frame_golden_keeps_tab_and_error_state_non_color_cues() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = model();
        app.capabilities.color = ColorSupport::NoColor;
        app.apply_runtime(crate::RuntimeUpdate::ToolFinished {
            id: "missing".to_string(),
            name: "inspect".to_string(),
            is_error: true,
            output: String::new(),
        });
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw no-color frame");
        let buffer = terminal.backend().buffer();
        assert!(buffer[(0, 0)]
            .modifier
            .contains(ratatui::style::Modifier::REVERSED));
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains('×'));
    }
}
