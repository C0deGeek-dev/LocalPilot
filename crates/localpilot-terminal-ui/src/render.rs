use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Widget, Wrap};
use ratatui::Frame;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::CompletionKind;
use crate::{
    ActivityState, AppModel, DialogState, Focus, FrameLayout, ItemKind, PinnedPrompt, TabId,
    TextStyle, ThemeResolver, UiRole, VisualRow, VisualRowPart, APP_NAME, MINIMUM_HEIGHT,
    MINIMUM_WIDTH,
};

/// Six banner lines plus one deliberate blank line before the first prompt.
const BANNER_ROWS: u16 = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabHit {
    pub tab: TabId,
    pub area: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompletionHit {
    pub index: usize,
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

    #[must_use]
    pub fn content_start_for_thumb_top(&self, thumb_top: u16) -> Option<usize> {
        let thumb = self.thumb?;
        let max_thumb_start = usize::from(self.track.height.saturating_sub(thumb.height));
        let max_view_start = self.total_rows.saturating_sub(self.viewport_rows);
        if max_thumb_start == 0 || max_view_start == 0 {
            return Some(0);
        }
        let relative = usize::from(
            thumb_top
                .saturating_sub(self.track.y)
                .min(self.track.height.saturating_sub(thumb.height)),
        );
        Some(
            relative
                .saturating_mul(max_view_start)
                .saturating_add(max_thumb_start / 2)
                / max_thumb_start,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineRowHit {
    pub y: u16,
    pub content_x: u16,
    pub row: VisualRow,
}

impl TimelineRowHit {
    #[must_use]
    pub fn point_for_column(&self, column: u16, trailing: bool) -> crate::ContentPoint {
        crate::Timeline::point_for_column(
            &self.row,
            column.saturating_sub(self.content_x),
            trailing,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitMap {
    pub frame: Option<FrameLayout>,
    pub tabs: Vec<TabHit>,
    pub timeline: Rect,
    pub timeline_rows: Vec<TimelineRowHit>,
    pub completion_rows: Vec<CompletionHit>,
    pub scrollbar: ScrollbarGeometry,
    pub composer: Rect,
    pub editor_width: u16,
    pub composer_scroll: usize,
}

#[must_use]
pub fn render(frame: &mut Frame<'_>, app: &AppModel) -> HitMap {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(theme(app).ui(UiRole::Background)),
        area,
    );
    // FrameLayout insets the composer twice: once for the outer surface and
    // once for its content. Use that exact width for the height request so the
    // renderer never wraps with one width and allocates rows with another.
    let prospective_editor_width = area.width.saturating_sub(4).max(1);
    let requested_editor_rows = if let Some((search, _)) = reverse_search_projection(app) {
        crate::text::wrap_ranges(&search, prospective_editor_width)
            .len()
            .max(1)
    } else {
        app.editor
            .visual_rows(prospective_editor_width)
            .len()
            .max(1)
    };
    let requested_editor_rows = u16::try_from(requested_editor_rows).unwrap_or(u16::MAX);
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
            timeline_rows: Vec::new(),
            completion_rows: Vec::new(),
            scrollbar: ScrollbarGeometry::calculate(Rect::default(), 0, 0, 0),
            composer: Rect::default(),
            editor_width: 1,
            composer_scroll: 0,
        };
    };

    let tabs = render_tabs(frame, layout.tabs, app);
    let (scrollbar, mut timeline_rows) = render_timeline(frame, layout, app);
    let (completion_area, completion_rows) = render_completion(frame, layout.timeline_content, app);
    if let Some(completion_area) = completion_area {
        timeline_rows.retain(|hit| hit.y < completion_area.y);
    }
    render_status(frame, layout.status, app, layout.stacked);
    let (editor_width, composer_scroll) = render_composer(frame, layout, app);
    render_footer(frame, layout.footer, app, layout.stacked);
    render_dialog(frame, area, app);
    HitMap {
        frame: Some(layout),
        tabs,
        timeline: layout.timeline_content,
        timeline_rows,
        completion_rows,
        scrollbar,
        composer: layout.composer_content,
        editor_width,
        composer_scroll,
    }
}

fn render_completion(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &AppModel,
) -> (Option<Rect>, Vec<CompletionHit>) {
    let Some(completion) = app.completion() else {
        return (None, Vec::new());
    };
    let visible = completion
        .items
        .len()
        .clamp(1, 8)
        .min(usize::from(area.height));
    if visible == 0 {
        return (None, Vec::new());
    }
    let height = u16::try_from(visible).unwrap_or(area.height);
    let picker = Rect::new(
        area.x,
        area.bottom().saturating_sub(height),
        area.width,
        height,
    );
    frame.render_widget(Clear, picker);
    frame.render_widget(
        Block::default().style(theme(app).ui(UiRole::Surface)),
        picker,
    );

    if completion.items.is_empty() {
        let message = match (completion.kind, completion.loading) {
            (CompletionKind::File, true) => "  Indexing workspace files…",
            (CompletionKind::File, false) => "  No matching files",
            (CompletionKind::Command, _) => "  No matching commands",
        };
        frame.render_widget(
            Paragraph::new(message).style(theme(app).ui(UiRole::Muted)),
            picker,
        );
        return (Some(picker), Vec::new());
    }

    let start = completion
        .selected
        .saturating_add(1)
        .saturating_sub(visible);
    let mut hits = Vec::with_capacity(visible);
    for (offset, (index, item)) in completion
        .items
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .enumerate()
    {
        let row = Rect::new(
            picker.x,
            picker
                .y
                .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX)),
            picker.width,
            1,
        );
        let selected = index == completion.selected;
        let prefix = if selected { "❯ " } else { "  " };
        let label = match completion.kind {
            CompletionKind::Command => format!("/{name}", name = item.name),
            CompletionKind::File => format!("@{name}", name = item.name),
        };
        let fixed =
            UnicodeWidthStr::width(prefix).saturating_add(UnicodeWidthStr::width(label.as_str()));
        let description_budget = usize::from(row.width).saturating_sub(fixed.saturating_add(2));
        let description = truncate_end(
            &item.description,
            u16::try_from(description_budget).unwrap_or(u16::MAX),
        );
        let gap = usize::from(row.width)
            .saturating_sub(fixed.saturating_add(UnicodeWidthStr::width(description.as_str())));
        let line = Line::from(vec![
            Span::styled(
                prefix,
                if selected {
                    theme(app).ui(UiRole::Focus)
                } else {
                    theme(app).ui(UiRole::Muted)
                },
            ),
            Span::styled(label, theme(app).ui(UiRole::Foreground)),
            Span::raw(" ".repeat(gap)),
            Span::styled(description, theme(app).ui(UiRole::Muted)),
        ])
        .style(theme(app).ui(UiRole::Surface));
        line.render(row, frame.buffer_mut());
        hits.push(CompletionHit { index, area: row });
    }
    (Some(picker), hits)
}

fn render_tabs(frame: &mut Frame<'_>, area: Rect, app: &AppModel) -> Vec<TabHit> {
    let theme = theme(app);
    let mut hits = Vec::new();
    let mut x = area.x.saturating_add(2);
    let right = area.right().saturating_sub(2);
    for tab in &app.tabs {
        let label = format!(" {} ", tab.label());
        let width = u16::try_from(UnicodeWidthStr::width(label.as_str())).unwrap_or(u16::MAX);
        if width > right.saturating_sub(x) {
            let arrow = Rect::new(right.saturating_sub(1), area.y, 1, 1);
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
) -> (ScrollbarGeometry, Vec<TimelineRowHit>) {
    let area = layout.timeline_content;
    let view = app.timeline.view(area.width.max(1), area.height.max(1));
    let banner_visible = view.pinned.is_none()
        && view.start == 0
        && (view.total_rows.saturating_add(usize::from(BANNER_ROWS)) <= usize::from(area.height)
            || matches!(app.timeline.viewport, crate::ViewportAnchor::Top));
    let content_offset = if banner_visible {
        render_idle_banner(frame, area, app);
        BANNER_ROWS
    } else if let Some(pinned) = &view.pinned {
        render_pinned_prompt(frame, area, pinned, app);
        u16::try_from(PinnedPrompt::ROWS).unwrap_or(u16::MAX)
    } else {
        0
    };
    let mut row_hits = Vec::new();
    if view.rows.is_empty() && view.pinned.is_none() && !banner_visible {
        render_idle_banner(frame, area, app);
    } else {
        let capacity = usize::from(area.height.saturating_sub(content_offset));
        for (offset, row) in view.rows.iter().take(capacity).enumerate() {
            let y = area
                .y
                .saturating_add(content_offset)
                .saturating_add(offset as u16);
            timeline_line(row, app, area.width)
                .render(Rect::new(area.x, y, area.width, 1), frame.buffer_mut());
            if matches!(row.part, VisualRowPart::Content { .. }) {
                row_hits.push(TimelineRowHit {
                    y,
                    content_x: area.x.saturating_add(row.content_column),
                    row: row.clone(),
                });
            }
        }
    }

    let scrollbar_viewport_rows = if banner_visible {
        view.viewport_rows.saturating_sub(usize::from(BANNER_ROWS))
    } else {
        view.viewport_rows
    };
    let scrollbar = ScrollbarGeometry::calculate(
        layout.scrollbar,
        view.start,
        view.total_rows,
        scrollbar_viewport_rows,
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
                Paragraph::new("█").style(theme.ui(UiRole::Focus)),
                Rect::new(thumb.x, y, 1, 1),
            );
        }
    }
    (scrollbar, row_hits)
}

fn render_idle_banner(frame: &mut Frame<'_>, area: Rect, app: &AppModel) {
    let theme = theme(app);
    let mark = theme.ui(UiRole::Accent);
    let rows = vec![
        Line::from(vec![
            Span::styled("╭──────╮", mark),
            Span::styled(
                format!(
                    "  {APP_NAME} v{}",
                    app.header
                        .version
                        .strip_prefix('v')
                        .unwrap_or(&app.header.version)
                ),
                theme.ui(UiRole::Foreground),
            ),
        ]),
        Line::from(vec![
            Span::styled("│ >_ ● │", mark),
            Span::styled(
                format!("  {} · {}", app.header.provider, app.header.model),
                theme.ui(UiRole::Muted),
            ),
        ]),
        Line::from(vec![
            Span::styled("╰──┬───╯", mark),
            Span::styled(
                "  Local-first assistance for this workspace",
                theme.ui(UiRole::Muted),
            ),
        ]),
        Line::default(),
        Line::from(vec![
            Span::styled("● ", theme.ui(UiRole::Accent)),
            Span::styled("Tip: press ? for shortcuts", theme.ui(UiRole::Foreground)),
        ]),
        Line::from(vec![
            Span::styled("  └ ", theme.ui(UiRole::Muted)),
            Span::styled("Type / to browse commands", theme.ui(UiRole::Muted)),
        ]),
    ];
    for (offset, row) in rows.into_iter().take(usize::from(area.height)).enumerate() {
        row.render(
            Rect::new(area.x, area.y.saturating_add(offset as u16), area.width, 1),
            frame.buffer_mut(),
        );
    }
}

fn render_pinned_prompt(frame: &mut Frame<'_>, area: Rect, pin: &PinnedPrompt, app: &AppModel) {
    let theme = theme(app);
    let glyph = if pin.overflowing { "↓ " } else { "❯ " };
    let trailing = pin.trailing.as_deref().unwrap_or("");
    let pending = if pin.pending { " (pending)" } else { "" };
    let fixed_width = 1usize
        .saturating_add(UnicodeWidthStr::width(glyph))
        .saturating_add(UnicodeWidthStr::width(pending))
        .saturating_add(UnicodeWidthStr::width(trailing))
        .saturating_add(1);
    let text_budget = usize::from(area.width).saturating_sub(fixed_width);
    let text = truncate_end(&pin.text, u16::try_from(text_budget).unwrap_or(u16::MAX));
    let used = fixed_width.saturating_add(UnicodeWidthStr::width(text.as_str()));
    let gap = usize::from(area.width).saturating_sub(used);
    let line = Line::from(vec![
        Span::styled(" ", theme.ui(UiRole::Surface)),
        Span::styled(
            glyph,
            theme.text(TextStyle::new(crate::SemanticRole::User).bold()),
        ),
        Span::styled(
            text,
            theme.text(TextStyle::new(crate::SemanticRole::User).bold()),
        ),
        Span::styled(
            pending,
            theme.text(TextStyle::new(crate::SemanticRole::User).bold()),
        ),
        Span::raw(" ".repeat(gap)),
        Span::styled(trailing.to_string(), theme.ui(UiRole::Muted)),
        Span::styled(" ", theme.ui(UiRole::Surface)),
    ])
    .style(theme.ui(UiRole::Surface));
    framed_rule(area.width, true, theme.ui(UiRole::SurfaceEdge))
        .render(Rect::new(area.x, area.y, area.width, 1), frame.buffer_mut());
    line.render(
        Rect::new(area.x, area.y.saturating_add(1), area.width, 1),
        frame.buffer_mut(),
    );
    framed_rule(area.width, false, theme.ui(UiRole::SurfaceEdge)).render(
        Rect::new(area.x, area.y.saturating_add(2), area.width, 1),
        frame.buffer_mut(),
    );
}

fn timeline_line(row: &VisualRow, app: &AppModel, width: u16) -> Line<'static> {
    let theme = theme(app);
    if row.part == VisualRowPart::FrameTop {
        return framed_rule(width, true, theme.ui(UiRole::SurfaceEdge));
    }
    if row.part == VisualRowPart::FrameBottom {
        return framed_rule(width, false, theme.ui(UiRole::SurfaceEdge));
    }

    let VisualRowPart::Content { first, last } = row.part else {
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
            let selected = app
                .timeline
                .selection_contains_grapheme(row.item_id, start, end);
            let mut style = if row.kind == ItemKind::User {
                theme.text(visual_span.style.bold())
            } else {
                theme.text(visual_span.style)
            };
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
        let pending = if row.pending && last {
            " (pending)"
        } else {
            ""
        };
        let reserved = prefix_width
            .saturating_add(content_width)
            .saturating_add(UnicodeWidthStr::width(pending))
            .saturating_add(trailing_width)
            .saturating_add(1);
        if !pending.is_empty() {
            spans.push(Span::styled(
                pending,
                theme.text(TextStyle::new(crate::SemanticRole::User).bold()),
            ));
        }
        let gap = usize::from(width).saturating_sub(reserved);
        if gap > 0 {
            spans.push(Span::raw(" ".repeat(gap)));
        }
        if !trailing.is_empty() {
            spans.push(Span::styled(trailing.to_string(), theme.ui(UiRole::Muted)));
        }
        spans.push(Span::styled(" ", theme.ui(UiRole::Surface)));
    }
    let line = Line::from(spans);
    if row.kind == ItemKind::User {
        line.style(theme.ui(UiRole::Surface))
    } else {
        line
    }
}

fn framed_rule(width: u16, top: bool, style: ratatui::style::Style) -> Line<'static> {
    let fill = if top { "▄" } else { "▀" };
    Line::styled(fill.repeat(usize::from(width)), style)
}

fn role_prefix(
    kind: ItemKind,
    activity: Option<ActivityState>,
    first: bool,
    theme: ThemeResolver,
) -> Vec<Span<'static>> {
    match kind {
        ItemKind::User => vec![
            Span::styled(" ", theme.ui(UiRole::Surface)),
            Span::styled(
                if first { "❯ " } else { "  " },
                theme.text(TextStyle::new(crate::SemanticRole::User).bold()),
            ),
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
    let area = inset_chrome(area);
    let theme = theme(app);
    let left = status_left(app);
    let right = status_right(app);
    if narrow {
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    middle_elide(&left, area.width),
                    theme.ui(UiRole::Foreground),
                ),
                Line::styled(truncate_end(&right, area.width), theme.ui(UiRole::Muted)),
            ]),
            area,
        );
    } else {
        frame.render_widget(
            Paragraph::new(two_sided(&left, &right, area.width)).style(theme.ui(UiRole::Muted)),
            area,
        );
    }
}

fn status_left(app: &AppModel) -> String {
    app.header.branch.as_ref().map_or_else(
        || app.header.workspace.clone(),
        |branch| {
            format!(
                "{} [{}{}]",
                app.header.workspace,
                branch,
                if app.header.workspace_dirty == Some(true) {
                    "*"
                } else {
                    ""
                }
            )
        },
    )
}

fn status_right(app: &AppModel) -> String {
    let (input, output) = app.usage.unwrap_or_default();
    let mut parts = vec![format!("{} tokens", input.saturating_add(output))];
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
    let left_edge = if app.focus == Focus::Composer {
        theme.ui(UiRole::Focus)
    } else {
        theme.ui(UiRole::SurfaceEdge)
    };
    let surface_edge = theme.ui(UiRole::SurfaceEdge);
    let surface = theme.ui(UiRole::Surface);
    let inner = layout.composer_content;
    let width = inner.width.max(1);
    if let Some((search, search_cursor)) = reverse_search_projection(app) {
        let (cursor_row, cursor_column) =
            crate::editor::text_row_and_column(&search, search_cursor, width);
        let visible_rows = usize::from(inner.height.max(1));
        let scroll = cursor_row.saturating_add(1).saturating_sub(visible_rows);
        render_slim_frame(frame, layout.composer, surface_edge, left_edge, surface);
        frame.render_widget(
            Paragraph::new(search)
                .style(surface)
                .wrap(Wrap { trim: false })
                .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
            inner,
        );
        if app.focus == Focus::Composer && app.dialog.is_none() {
            let cursor_y = inner
                .y
                .saturating_add(
                    u16::try_from(cursor_row.saturating_sub(scroll)).unwrap_or(u16::MAX),
                )
                .min(inner.bottom().saturating_sub(1));
            let cursor_x = inner
                .x
                .saturating_add(cursor_column)
                .min(inner.right().saturating_sub(1));
            frame.set_cursor_position(Position::new(cursor_x, cursor_y));
        }
        return (width, scroll);
    }
    let (cursor_row, cursor_column) = app.editor.cursor_row_and_column(width);
    let visible_rows = usize::from(inner.height.max(1));
    let scroll = cursor_row.saturating_add(1).saturating_sub(visible_rows);
    render_slim_frame(frame, layout.composer, surface_edge, left_edge, surface);
    frame.render_widget(
        Paragraph::new(app.editor.text().to_string())
            .style(surface)
            .wrap(Wrap { trim: false })
            .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
        inner,
    );
    if app.focus == Focus::Composer && app.dialog.is_none() {
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

fn reverse_search_projection(app: &AppModel) -> Option<(String, usize)> {
    let search = app.reverse_search()?;
    let result = if search.has_match {
        app.editor.text()
    } else {
        "no match"
    };
    let prefix = "(history-search)`";
    let cursor = prefix.len().saturating_add(search.query.len());
    Some((format!("{prefix}{}': {result}", search.query), cursor))
}

fn render_dialog(frame: &mut Frame<'_>, frame_area: Rect, app: &AppModel) {
    let Some(dialog) = &app.dialog else {
        return;
    };
    let width = frame_area.width.saturating_sub(4).min(72);
    let height = frame_area.height.saturating_sub(2).min(7);
    if width < 20 || height < 5 {
        return;
    }
    let area = Rect::new(
        frame_area.x + frame_area.width.saturating_sub(width) / 2,
        frame_area.y + frame_area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let inner = Rect::new(
        area.x.saturating_add(2),
        area.y.saturating_add(1),
        area.width.saturating_sub(4),
        area.height.saturating_sub(2),
    );
    let theme = theme(app);
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(theme.ui(UiRole::Surface)), area);
    let lines = match dialog {
        DialogState::Trust { path } => vec![
            Line::from(vec![
                Span::styled("● ", theme.ui(UiRole::Accent)),
                Span::styled("Trust this workspace?", theme.ui(UiRole::Prompt)),
            ]),
            Line::styled(
                middle_elide(path, inner.width),
                theme.ui(UiRole::Foreground),
            ),
            Line::styled(
                "LocalPilot will use the permissions in the active profile.",
                theme.ui(UiRole::Muted),
            ),
            Line::styled("Y trust and continue · N exit", theme.ui(UiRole::Muted)),
        ],
        DialogState::Approval {
            tool,
            target,
            risk_class,
        } => vec![
            Line::from(vec![
                Span::styled("● ", theme.ui(UiRole::Warning)),
                Span::styled("Permission required", theme.ui(UiRole::Prompt)),
            ]),
            Line::styled(
                format!("{tool} · {risk_class}"),
                theme.ui(UiRole::Foreground),
            ),
            Line::styled(target.clone(), theme.ui(UiRole::Muted)),
            Line::styled("Y allow once · N deny", theme.ui(UiRole::Muted)),
        ],
    };
    frame.render_widget(
        Paragraph::new(lines)
            .style(theme.ui(UiRole::Surface))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

fn render_slim_frame(
    frame: &mut Frame<'_>,
    area: Rect,
    surface_edge: ratatui::style::Style,
    left_edge: ratatui::style::Style,
    surface: ratatui::style::Style,
) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    framed_composer_rule(area.width, true, surface_edge)
        .render(Rect::new(area.x, area.y, area.width, 1), frame.buffer_mut());
    framed_composer_rule(area.width, false, surface_edge).render(
        Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
        frame.buffer_mut(),
    );
    for y in area.y.saturating_add(1)..area.bottom().saturating_sub(1) {
        frame.render_widget(
            Paragraph::new("").style(surface),
            Rect::new(area.x, y, area.width, 1),
        );
        frame.render_widget(
            Paragraph::new("▏").style(left_edge),
            Rect::new(area.x, y, 1, 1),
        );
    }
}

fn framed_composer_rule(
    width: u16,
    top: bool,
    surface_edge: ratatui::style::Style,
) -> Line<'static> {
    let fill = if top { "▄" } else { "▀" };
    Line::styled(fill.repeat(usize::from(width)), surface_edge)
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &AppModel, narrow: bool) {
    let area = inset_chrome(area);
    let state = footer_state(app);
    let shortcuts = "? help · / commands";
    let context = if matches!(app.work, crate::WorkState::Busy { .. }) {
        app.header.model.clone()
    } else {
        format!(
            "{} · {} → {}",
            app.header.mode, app.header.profile, app.header.model
        )
    };
    let busy = matches!(app.work, crate::WorkState::Busy { .. });
    let theme = theme(app);
    let text = if narrow {
        format!(
            "{}\n{}",
            truncate_end(&state, area.width),
            two_sided(if busy { "" } else { shortcuts }, &context, area.width)
        )
    } else {
        let left = if busy {
            state.clone()
        } else {
            format!("{state} · {shortcuts}")
        };
        two_sided(&left, &context, area.width)
    };
    frame.render_widget(Paragraph::new(text).style(theme.ui(UiRole::Muted)), area);
    if let Some(offset) = state.find("● Working") {
        let x = area
            .x
            .saturating_add(u16::try_from(UnicodeWidthStr::width(&state[..offset])).unwrap_or(0));
        frame.render_widget(
            Paragraph::new("● Working").style(theme.ui(UiRole::Accent)),
            Rect::new(x, area.y, 9.min(area.right().saturating_sub(x)), 1),
        );
    }
}

fn inset_chrome(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(1),
        area.y,
        area.width.saturating_sub(3),
        area.height,
    )
}

fn footer_state(app: &AppModel) -> String {
    let held = !matches!(app.timeline.viewport, crate::ViewportAnchor::FollowBottom);
    let new_output = app.timeline.has_new_content();
    if app.exit_armed {
        return "press Ctrl+C again to exit".to_string();
    }
    if app.reverse_search().is_some() {
        return "history search · type to filter · Esc keep match".to_string();
    }
    if let Some(completion) = app.completion() {
        let fallback = match (completion.kind, completion.loading) {
            (CompletionKind::File, true) => "indexing workspace files",
            (CompletionKind::File, false) => "file completion",
            (CompletionKind::Command, _) => "command completion",
        };
        let detail = completion
            .items
            .get(completion.selected)
            .map_or(fallback, |item| item.description.as_str());
        return format!("{detail} · ↑↓ navigate · Enter/Tab accept · Esc close");
    }
    match (app.work, app.exit_armed) {
        (_, true) => "press Ctrl+C again to exit".to_string(),
        (crate::WorkState::Idle, false) if held && new_output => {
            "↓ new output · timeline held · Ctrl+C twice to exit".to_string()
        }
        (crate::WorkState::Idle, false) if held => {
            "timeline held · Ctrl+C twice to exit".to_string()
        }
        (crate::WorkState::Idle, false) => "idle · Ctrl+C copy / twice to exit".to_string(),
        (
            crate::WorkState::Busy {
                cancellation_requested: false,
            },
            false,
        ) if held && new_output => {
            format!(
                "↓ new output · ● Working · {} · Esc interrupt",
                format_stream_size(app.stream_bytes)
            )
        }
        (
            crate::WorkState::Busy {
                cancellation_requested: false,
            },
            false,
        ) if held => format!(
            "timeline held · ● Working · {} · Esc interrupt",
            format_stream_size(app.stream_bytes)
        ),
        (
            crate::WorkState::Busy {
                cancellation_requested: false,
            },
            false,
        ) => format!(
            "● Working · {} · Esc interrupt",
            format_stream_size(app.stream_bytes)
        ),
        (
            crate::WorkState::Busy {
                cancellation_requested: true,
            },
            false,
        ) => "cancelling · Ctrl+C again to exit".to_string(),
    }
}

fn format_stream_size(bytes: usize) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= MIB {
        format!("{:.1} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes / KIB)
    } else {
        format!("{} B", bytes as usize)
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
                branch: Some("main".to_string()),
                workspace_dirty: Some(false),
                mode: "agent".to_string(),
                profile: "default".to_string(),
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
        assert!(rendered.contains("Tip: press ? for shortcuts"));
        assert!(rendered.contains("Type / to browse commands"));
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
    fn trust_and_approval_dialogs_use_original_deny_safe_copy() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = model();
        app.require_workspace_trust("D:\\workspace");
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw trust dialog");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Trust this workspace?"));
        assert!(rendered.contains("Y trust and continue · N exit"));

        app.request_approval("write_file", "D:\\workspace\\src\\main.rs", "write");
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw approval dialog");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Permission required"));
        assert!(rendered.contains("Y allow once · N deny"));
    }

    #[test]
    fn banner_remains_the_scrollable_conversation_header() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = model();
        let _ = app.append_prompt("first prompt", Some("12:34".to_string()), false);
        let _ = app.timeline.push(ItemKind::Assistant, "short response");
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw conversation header");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("LocalPilot v0"));
        assert!(rendered.contains("first prompt"));
        assert!(rendered.contains("short response"));

        for number in 0..60 {
            let _ = app
                .timeline
                .push(ItemKind::Assistant, format!("response {number:02}"));
        }
        app.timeline.scroll_by(-10_000, 76, 16);
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw held conversation header");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("LocalPilot v0"));
        assert!(rendered.contains("first prompt"));
        let hit_map = hit_map.expect("hit map");
        let layout = hit_map.frame.expect("layout");
        let view = app.timeline.view(
            layout.timeline_content.width,
            layout.timeline_content.height,
        );
        assert_eq!(
            hit_map.scrollbar,
            ScrollbarGeometry::calculate(
                layout.scrollbar,
                view.start,
                view.total_rows,
                view.viewport_rows.saturating_sub(usize::from(BANNER_ROWS)),
            )
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
    fn in_flow_and_pinned_prompts_are_three_row_dark_surfaces() {
        let resolver = ThemeResolver::new(Theme::Default, ColorSupport::Color);

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = model();
        let prompt = app
            .timeline
            .push(ItemKind::User, "current prompt")
            .expect("prompt");
        assert!(app.timeline.set_pending(prompt, true));
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw in-flow prompt");
        let layout = hit_map.expect("hit map").frame.expect("layout");
        let buffer = terminal.backend().buffer();
        let prompt_y = layout.timeline_content.y + BANNER_ROWS;
        assert_eq!(buffer[(layout.timeline_content.x, prompt_y)].symbol(), "▄");
        assert!(buffer_line(buffer, prompt_y + 1).contains("current prompt (pending)"));
        assert_eq!(
            buffer[(layout.timeline_content.x, prompt_y + 1)].symbol(),
            " ",
            "prompt surfaces must not draw a visible side bar"
        );
        assert_eq!(
            buffer[(layout.timeline_content.right() - 1, prompt_y + 1)].symbol(),
            " ",
            "prompt surfaces must not draw a visible side bar"
        );
        assert_eq!(
            buffer[(layout.timeline_content.x + 1, prompt_y + 1)]
                .style()
                .bg,
            resolver.ui(UiRole::Surface).bg
        );
        assert_eq!(
            buffer[(layout.timeline_content.x, prompt_y + 2)].symbol(),
            "▀"
        );

        let mut app = model();
        let _ = app.timeline.push(ItemKind::User, "pinned prompt");
        for number in 0..80 {
            let _ = app
                .timeline
                .push(ItemKind::Assistant, format!("response {number:03}"));
        }
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw pinned prompt");
        let layout = hit_map.expect("hit map").frame.expect("layout");
        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer[(layout.timeline_content.x, layout.timeline_content.y)].symbol(),
            "▄"
        );
        assert!(buffer_line(buffer, layout.timeline_content.y + 1).contains("pinned prompt"));
        assert_eq!(
            buffer[(layout.timeline_content.x, layout.timeline_content.y + 2)].symbol(),
            "▀"
        );
        assert!(buffer_line(buffer, layout.timeline_content.y + 3).contains("response"));
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
    fn scrollbar_inverse_is_monotonic_and_reaches_both_ends() {
        let track = Rect::new(79, 3, 1, 20);
        let top = ScrollbarGeometry::calculate(track, 0, 500, 25);
        let top_y = top.thumb.expect("top thumb").y;
        assert_eq!(top.content_start_for_thumb_top(top_y), Some(0));

        let bottom = ScrollbarGeometry::calculate(track, 475, 500, 25);
        let bottom_y = bottom.thumb.expect("bottom thumb").y;
        assert_eq!(bottom.content_start_for_thumb_top(bottom_y), Some(475));

        let mut previous = 0;
        for y in track.y..track.bottom() {
            let start = bottom
                .content_start_for_thumb_top(y)
                .expect("visible thumb has an inverse");
            assert!(start >= previous);
            previous = start;
        }
    }

    #[test]
    fn timeline_row_hits_share_rendered_grapheme_coordinates() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = model();
        let _ = app.timeline.push(ItemKind::User, "prompt");
        let _ = app.timeline.push(ItemKind::Assistant, "alpha 界 beta");
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw row hits");
        let hit_map = hit_map.expect("hit map");
        let prompt = hit_map
            .timeline_rows
            .iter()
            .find(|hit| hit.row.kind == ItemKind::User)
            .expect("prompt hit");
        assert_eq!(prompt.content_x, hit_map.timeline.x + 3);

        let response = hit_map
            .timeline_rows
            .iter()
            .find(|hit| hit.row.kind == ItemKind::Assistant)
            .expect("response hit");
        assert_eq!(response.content_x, hit_map.timeline.x + 2);
        assert_eq!(response.point_for_column(response.content_x, false).byte, 0);
        assert_eq!(
            response
                .point_for_column(response.content_x + 6, false)
                .byte,
            "alpha ".len()
        );
        assert_eq!(
            response.point_for_column(response.content_x + 6, true).byte,
            "alpha 界".len()
        );
        assert_eq!(
            response.point_for_column(u16::MAX, true).byte,
            "alpha 界 beta".len()
        );
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
            assert_eq!(buffer[(layout.composer.x, layout.composer.y)].symbol(), "▄");
            assert_eq!(
                buffer[(layout.composer.x, layout.composer.bottom() - 1)].symbol(),
                "▀"
            );
            assert_eq!(
                buffer[(layout.scrollbar.x, layout.scrollbar.y)].symbol(),
                "│"
            );
            assert!(buffer_line(buffer, layout.status.y).contains("workspace"));
            assert!(buffer_line(buffer, layout.footer.y).contains("Ctrl+C"));
            if width == 40 {
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
            let actual = terminal.backend().buffer()[(2, 0)].style();
            let expected =
                ThemeResolver::new(theme_name, ColorSupport::Color).ui(UiRole::TabActive);
            assert_eq!(actual.fg, expected.fg);
            assert_eq!(actual.bg, expected.bg);
            assert!(actual.add_modifier.contains(expected.add_modifier));
        }
    }

    #[test]
    fn focused_composer_forms_a_dark_surface_with_one_subtle_focus_edge() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let app = model();
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw focused composer");
        let layout = hit_map.expect("hit map").frame.expect("frame layout");
        let buffer = terminal.backend().buffer();
        let resolver = ThemeResolver::new(Theme::Default, ColorSupport::Color);

        let rule = buffer[(layout.composer.x + 1, layout.composer.y)].style();
        let side = buffer[(layout.composer.x, layout.composer_content.y)].style();
        assert_eq!(
            buffer[(layout.composer.x, layout.composer_content.y)].symbol(),
            "▏"
        );
        assert_eq!(
            buffer[(layout.composer.right() - 1, layout.composer_content.y)].symbol(),
            " ",
            "the filled composer must not draw a right-edge artifact"
        );
        assert_eq!(
            rule.fg,
            resolver.ui(UiRole::SurfaceEdge).fg,
            "the half-block fill must merge into the input surface"
        );
        assert_eq!(
            side.fg,
            resolver.ui(UiRole::Focus).fg,
            "focus is carried only by the thin side"
        );
        assert_eq!(
            buffer[(layout.composer_content.x, layout.composer_content.y)]
                .style()
                .bg,
            resolver.ui(UiRole::Surface).bg
        );
        assert_ne!(
            resolver.ui(UiRole::Focus).fg,
            resolver.ui(UiRole::Accent).fg
        );
    }

    #[test]
    fn reverse_history_search_replaces_the_composer_without_moving_the_timeline() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = model();
        app.seed_history(vec!["remember this prompt".to_string()]);
        let _ = app.handle_input(crate::InputAction::OpenReverseHistory, 76);
        let _ = app.handle_input(crate::InputAction::Insert("this".to_string()), 76);
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw reverse search");
        let layout = hit_map.expect("hit map").frame.expect("frame layout");
        let line = buffer_line(terminal.backend().buffer(), layout.composer_content.y);
        assert!(line.contains("(history-search)`this': remember this prompt"));
        assert!(footer_state(&app).contains("Esc keep match"));
    }

    #[test]
    fn command_completion_floats_above_composer_and_reports_candidate_hits() {
        let mut app = model();
        app.set_command_catalog([
            crate::CompletionCommand {
                name: "model".to_string(),
                description: "Switch provider or model".to_string(),
            },
            crate::CompletionCommand {
                name: "memory".to_string(),
                description: "Inspect memory".to_string(),
            },
        ]);
        let _ = app.handle_input(crate::InputAction::Insert("/mo".to_string()), 76);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw completion");
        let hit_map = hit_map.expect("hit map");
        assert_eq!(hit_map.completion_rows.len(), 2);
        assert!(hit_map
            .completion_rows
            .iter()
            .all(|hit| hit.area.bottom() <= hit_map.frame.expect("layout").status.y));
        let selected = buffer_line(
            terminal.backend().buffer(),
            hit_map.completion_rows[0].area.y,
        );
        assert!(selected.contains("❯ /model"));
        assert!(selected.contains("Switch provider or model"));
    }

    #[test]
    fn file_completion_reports_indexing_then_renders_at_prefixed_paths() {
        let mut app = model();
        let _ = app.handle_input(crate::InputAction::Insert("@sam".to_string()), 76);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw indexing mention");
        let indexing = (0..24)
            .map(|row| buffer_line(terminal.backend().buffer(), row))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(indexing.contains("Indexing workspace files"));

        app.set_workspace_files(["src/sample.rs".to_string()]);
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw ready mention");
        let hit_map = hit_map.expect("hit map");
        let line = buffer_line(
            terminal.backend().buffer(),
            hit_map.completion_rows[0].area.y,
        );
        assert!(line.contains("❯ @src/sample.rs"));
    }

    #[test]
    fn composer_height_and_text_use_the_same_inset_width() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = model();
        app.editor.insert(&"x".repeat(77));
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw wrapped composer");
        let hit_map = hit_map.expect("hit map");
        assert_eq!(hit_map.editor_width, 76);
        assert_eq!(
            hit_map.frame.expect("frame layout").composer_content.height,
            2
        );
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
        assert!(buffer[(2, 0)]
            .modifier
            .contains(ratatui::style::Modifier::REVERSED));
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains('×'));
    }

    #[test]
    fn status_and_footer_render_only_truthful_workspace_and_session_context() {
        for (width, height) in [(120, 30), (40, 20)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            let mut app = model();
            app.header.workspace = "D:\\repos\\LocalX\\LocalPilot".to_string();
            app.header.branch = Some("terminal-chat-experience".to_string());
            app.header.workspace_dirty = Some(true);
            app.header.mode = "agent".to_string();
            app.header.profile = "relaxed".to_string();
            app.usage = Some((12, 34));
            app.context_usage = Some((2_500, 10_000));
            let mut hit_map = None;
            terminal
                .draw(|frame| hit_map = Some(render(frame, &app)))
                .expect("draw truthful context");
            let layout = hit_map.expect("hit map").frame.expect("layout");
            let buffer = terminal.backend().buffer();
            let status_left = buffer_line(buffer, layout.status.y);
            let status_right = buffer_line(buffer, layout.status.bottom() - 1);
            let footer_context = buffer_line(buffer, layout.footer.bottom() - 1);

            assert!(status_right.contains("46 tokens · 25% context"));
            assert!(footer_context.contains("agent · relaxed → model"));
            if width == 120 {
                assert!(status_left.contains("D:\\repos\\LocalX\\LocalPilot"));
                assert!(status_left.contains("[terminal-chat-experience*]"));
            } else {
                assert_eq!(layout.status.height, 2);
                assert_eq!(layout.footer.height, 2);
                assert!(status_left.contains("experience*]"));
            }
        }
    }

    #[test]
    fn working_footer_shows_truthful_stream_size_and_escape_interrupt() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = model();
        app.begin_work();
        app.apply_runtime(crate::RuntimeUpdate::Text("x".repeat(9_114)));
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw working footer");
        let layout = hit_map.expect("hit map").frame.expect("layout");
        let buffer = terminal.backend().buffer();
        let footer = buffer_line(buffer, layout.footer.y);

        assert!(footer.contains("● Working · 8.9 KiB · Esc interrupt"));
        assert!(footer.trim_end().ends_with("model"));
        assert!(!footer.contains("agent · default"));
        assert!(!footer.contains("? help"));
        assert_eq!(
            buffer[(layout.footer.x + 1, layout.footer.y)].style().fg,
            ThemeResolver::new(Theme::Default, ColorSupport::Color)
                .ui(UiRole::Accent)
                .fg
        );
    }

    #[test]
    fn held_stream_surfaces_new_output_without_moving_the_anchor() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = model();
        for number in 0..40 {
            let _ = app
                .timeline
                .push(ItemKind::Assistant, format!("response {number:03}"));
        }
        app.timeline.scroll_by(-20, 70, 12);
        let crate::ViewportAnchor::Held(anchor) = app.timeline.viewport else {
            panic!("timeline must be held");
        };
        let tail = app.timeline.items().last().expect("tail").id;
        assert!(app.timeline.append_text(tail, " streamed"));
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw held stream");
        assert_eq!(app.timeline.viewport, crate::ViewportAnchor::Held(anchor));
        assert!(terminal.backend().to_string().contains("new output"));
    }
}
