use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Widget, Wrap};
use ratatui::Frame;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::{CompletionKind, DiffPane, TakeoverView};
use crate::{
    ActivityState, AppModel, DialogState, Focus, FrameLayout, ItemKind, PinnedPrompt, TabId,
    TakeoverKind, TextStyle, Theme, ThemeResolver, UiRole, VisualRow, VisualRowPart, APP_NAME,
    MINIMUM_HEIGHT, MINIMUM_WIDTH,
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
pub struct ThemeHit {
    pub index: usize,
    pub area: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TakeoverHit {
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
    pub takeover: bool,
    pub frame: Option<FrameLayout>,
    pub tabs: Vec<TabHit>,
    pub timeline: Rect,
    pub timeline_rows: Vec<TimelineRowHit>,
    pub completion_rows: Vec<CompletionHit>,
    pub theme_rows: Vec<ThemeHit>,
    pub takeover_rows: Vec<TakeoverHit>,
    pub takeover_file_rows: Vec<TakeoverHit>,
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
    if let Some(takeover) = app.takeover() {
        return render_takeover(frame, area, app, takeover);
    }
    // FrameLayout insets the composer twice: once for the outer surface and
    // once for its content. Use that exact width for the height request so the
    // renderer never wraps with one width and allocates rows with another.
    let prospective_editor_width = area.width.saturating_sub(4).max(1);
    let requested_editor_rows =
        if let Some((search, _)) = input_overlay_projection(app, prospective_editor_width) {
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
            takeover: false,
            frame: None,
            tabs: Vec::new(),
            timeline: Rect::default(),
            timeline_rows: Vec::new(),
            completion_rows: Vec::new(),
            theme_rows: Vec::new(),
            takeover_rows: Vec::new(),
            takeover_file_rows: Vec::new(),
            scrollbar: ScrollbarGeometry::calculate(Rect::default(), 0, 0, 0),
            composer: Rect::default(),
            editor_width: 1,
            composer_scroll: 0,
        };
    };

    let tabs = render_tabs(frame, layout.tabs, app);
    let (scrollbar, mut timeline_rows) = render_timeline(frame, layout, app);
    let quick_help_area = render_quick_help(frame, layout.timeline_content, app);
    let (completion_area, completion_rows) = render_completion(frame, layout.timeline_content, app);
    if let Some(overlay_area) = completion_area.or(quick_help_area) {
        timeline_rows.retain(|hit| hit.y < overlay_area.y);
    }
    render_status(frame, layout.status, app, layout.stacked);
    let (editor_width, composer_scroll) = render_composer(frame, layout, app);
    render_footer(frame, layout.footer, app, layout.stacked);
    let theme_rows = render_theme_picker(frame, area, app);
    render_dialog(frame, area, app);
    HitMap {
        takeover: false,
        frame: Some(layout),
        tabs,
        timeline: layout.timeline_content,
        timeline_rows,
        completion_rows,
        theme_rows,
        takeover_rows: Vec::new(),
        takeover_file_rows: Vec::new(),
        scrollbar,
        composer: layout.composer_content,
        editor_width,
        composer_scroll,
    }
}

fn render_quick_help(frame: &mut Frame<'_>, area: Rect, app: &AppModel) -> Option<Rect> {
    if !app.quick_help() || area.height == 0 {
        return None;
    }
    let left = [
        "Enter       send prompt",
        "Shift+Enter add a line",
        "↑ / ↓       edit, then history",
        "Ctrl+R      search history",
        "Ctrl+G      external editor",
    ];
    let right = if app.capabilities.mouse_capture {
        [
            "Page Up/Down scroll timeline",
            "Wheel        scroll timeline",
            "Drag         select text",
            "Ctrl+F       search messages",
            "Esc          stop and steer",
        ]
    } else {
        [
            "Page Up/Down scroll timeline",
            "Mouse        disabled",
            "Ctrl+C       copy / exit",
            "Ctrl+F       search messages",
            "Esc          stop and steer",
        ]
    };
    let wide = area.width >= 70;
    let requested = if wide { 6 } else { 11 };
    let height = requested.min(area.height);
    let help = Rect::new(
        area.x,
        area.bottom().saturating_sub(height),
        area.width,
        height,
    );
    let theme = theme(app);
    frame.render_widget(Clear, help);
    frame.render_widget(Block::default().style(theme.ui(UiRole::Surface)), help);
    frame.render_widget(
        Paragraph::new("Quick help").style(theme.ui(UiRole::Accent)),
        Rect::new(
            help.x.saturating_add(2),
            help.y,
            help.width.saturating_sub(4),
            1,
        ),
    );
    if wide {
        let column_width = help.width.saturating_sub(5) / 2;
        for (index, (left, right)) in left.iter().zip(right.iter()).enumerate() {
            let row = u16::try_from(index).unwrap_or(u16::MAX);
            let y = help.y.saturating_add(1).saturating_add(row);
            frame.render_widget(
                Paragraph::new(*left).style(theme.ui(UiRole::Foreground)),
                Rect::new(help.x.saturating_add(2), y, column_width, 1),
            );
            frame.render_widget(
                Paragraph::new(*right).style(theme.ui(UiRole::Foreground)),
                Rect::new(
                    help.x.saturating_add(3).saturating_add(column_width),
                    y,
                    column_width,
                    1,
                ),
            );
        }
    } else {
        for (index, text) in left.iter().chain(right.iter()).enumerate() {
            let row = u16::try_from(index).unwrap_or(u16::MAX);
            let y = help.y.saturating_add(1).saturating_add(row);
            if y >= help.bottom() {
                break;
            }
            frame.render_widget(
                Paragraph::new(*text).style(theme.ui(UiRole::Foreground)),
                Rect::new(help.x.saturating_add(2), y, help.width.saturating_sub(4), 1),
            );
        }
    }
    Some(help)
}

fn render_takeover(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &AppModel,
    takeover: TakeoverView<'_>,
) -> HitMap {
    if area.width < 20 || area.height < 8 {
        frame.render_widget(
            Paragraph::new(format!("{APP_NAME}\nresize to view help")).wrap(Wrap { trim: false }),
            area,
        );
        render_dialog(frame, area, app);
        return HitMap {
            takeover: true,
            frame: None,
            tabs: Vec::new(),
            timeline: Rect::default(),
            timeline_rows: Vec::new(),
            completion_rows: Vec::new(),
            theme_rows: Vec::new(),
            takeover_rows: Vec::new(),
            takeover_file_rows: Vec::new(),
            scrollbar: ScrollbarGeometry::calculate(Rect::default(), 0, 0, 0),
            composer: Rect::default(),
            editor_width: 1,
            composer_scroll: 0,
        };
    }

    let theme = theme(app);
    let title = match takeover.kind {
        TakeoverKind::Help => " Help ",
        TakeoverKind::Settings => " Settings ",
        TakeoverKind::Diff => " Diff ",
    };
    frame.render_widget(
        Paragraph::new(title).style(theme.ui(UiRole::TabActive)),
        Rect::new(
            area.x.saturating_add(1),
            area.y,
            u16::try_from(title.width()).unwrap_or(area.width),
            1,
        ),
    );

    let footer_height = if takeover.kind == TakeoverKind::Settings {
        2
    } else {
        1
    };
    let content = Rect::new(
        area.x.saturating_add(2),
        area.y.saturating_add(2),
        area.width.saturating_sub(5),
        area.height.saturating_sub(2 + footer_height),
    );
    let viewport_rows = usize::from(content.height);
    let (start, total_rows, scrollbar_rows, takeover_rows, takeover_file_rows) = match takeover.kind
    {
        TakeoverKind::Help => {
            let lines = help_lines(
                takeover,
                content.width,
                theme,
                app.capabilities.mouse_capture,
            );
            let maximum = lines.len().saturating_sub(viewport_rows);
            let start = takeover.scroll.min(maximum);
            frame.render_widget(
                Paragraph::new(
                    lines
                        .iter()
                        .skip(start)
                        .take(viewport_rows)
                        .cloned()
                        .collect::<Vec<_>>(),
                ),
                content,
            );
            (start, lines.len(), viewport_rows, Vec::new(), Vec::new())
        }
        TakeoverKind::Settings => {
            let maximum = takeover.settings.len().saturating_sub(viewport_rows);
            let start = takeover.scroll.min(maximum);
            let mut hits = Vec::new();
            for (offset, (index, setting)) in takeover
                .settings
                .iter()
                .enumerate()
                .skip(start)
                .take(viewport_rows)
                .enumerate()
            {
                let y = content
                    .y
                    .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX));
                let selected = index == takeover.selected;
                let prefix = if selected { "❯ " } else { "  " };
                let left = format!("{prefix}{} · {}", setting.section, setting.name);
                frame.render_widget(
                    Paragraph::new(two_sided(&left, &setting.value, content.width)).style(
                        theme.ui(if selected {
                            UiRole::TabActive
                        } else {
                            UiRole::Foreground
                        }),
                    ),
                    Rect::new(content.x, y, content.width, 1),
                );
                hits.push(TakeoverHit {
                    index,
                    area: Rect::new(content.x, y, content.width, 1),
                });
            }
            if let Some(selected) = takeover.settings.get(takeover.selected) {
                frame.render_widget(
                    Paragraph::new(truncate_end(
                        &selected.description,
                        area.width.saturating_sub(3),
                    ))
                    .style(theme.ui(UiRole::Muted)),
                    Rect::new(
                        area.x.saturating_add(1),
                        area.bottom().saturating_sub(2),
                        area.width.saturating_sub(3),
                        1,
                    ),
                );
            }
            (
                start,
                takeover.settings.len(),
                viewport_rows,
                hits,
                Vec::new(),
            )
        }
        TakeoverKind::Diff => render_diff_takeover(frame, content, app, takeover),
    };

    let scrollbar = ScrollbarGeometry::calculate(
        Rect::new(
            area.right().saturating_sub(2),
            area.y.saturating_add(1),
            1,
            area.height.saturating_sub(3),
        ),
        start,
        total_rows,
        scrollbar_rows,
    );
    draw_scrollbar(frame, scrollbar, app);
    frame.render_widget(
        Paragraph::new(match takeover.kind {
            TakeoverKind::Help => "↑/↓ scroll · Page Up/Page Down · Esc close",
            TakeoverKind::Settings => "↑/↓ select · Page Up/Page Down · Esc close",
            TakeoverKind::Diff => "↑/↓ navigate · ←/→ switch pane · t hide/show files · Esc close",
        })
        .style(theme.ui(UiRole::Muted)),
        Rect::new(
            area.x.saturating_add(1),
            area.bottom().saturating_sub(1),
            area.width.saturating_sub(3),
            1,
        ),
    );
    render_dialog(frame, area, app);

    HitMap {
        takeover: true,
        frame: None,
        tabs: Vec::new(),
        timeline: content,
        timeline_rows: Vec::new(),
        completion_rows: Vec::new(),
        theme_rows: Vec::new(),
        takeover_rows,
        takeover_file_rows,
        scrollbar,
        composer: Rect::default(),
        editor_width: 1,
        composer_scroll: 0,
    }
}

fn render_diff_takeover(
    frame: &mut Frame<'_>,
    content: Rect,
    app: &AppModel,
    takeover: TakeoverView<'_>,
) -> (usize, usize, usize, Vec<TakeoverHit>, Vec<TakeoverHit>) {
    let theme = theme(app);
    let show_tree = takeover.tree_visible && content.width >= 60;
    let tree_width = if show_tree {
        (content.width / 3).clamp(20, 34)
    } else {
        0
    };
    let tree = Rect::new(content.x, content.y, tree_width, content.height);
    let diff = if show_tree {
        Rect::new(
            content.x.saturating_add(tree_width).saturating_add(1),
            content.y,
            content.width.saturating_sub(tree_width).saturating_sub(1),
            content.height,
        )
    } else {
        content
    };
    let mut file_hits = Vec::new();
    let file_viewport_rows = usize::from(tree.height.saturating_sub(1));
    let file_start = takeover
        .file_scroll
        .min(takeover.diff_files.len().saturating_sub(file_viewport_rows));
    if show_tree {
        frame.render_widget(
            Paragraph::new(format!("Files ({})", takeover.diff_files.len()))
                .style(theme.ui(UiRole::Accent)),
            Rect::new(tree.x, tree.y, tree.width, 1),
        );
        for (offset, (index, file)) in takeover
            .diff_files
            .iter()
            .enumerate()
            .skip(file_start)
            .take(usize::from(tree.height.saturating_sub(1)))
            .enumerate()
        {
            let y = tree
                .y
                .saturating_add(1)
                .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX));
            let selected = index == takeover.selected_file;
            let prefix = if selected { "❯ " } else { "  " };
            let left = format!("{prefix}{} {}", file.status, file.path);
            let counts = format!("+{} -{}", file.additions, file.deletions);
            frame.render_widget(
                Paragraph::new(two_sided(&left, &counts, tree.width)).style(theme.ui(
                    if selected && takeover.diff_pane == DiffPane::Files {
                        UiRole::TabActive
                    } else {
                        UiRole::Foreground
                    },
                )),
                Rect::new(tree.x, y, tree.width, 1),
            );
            file_hits.push(TakeoverHit {
                index,
                area: Rect::new(tree.x, y, tree.width, 1),
            });
        }
        for y in tree.y..tree.bottom() {
            frame.render_widget(
                Paragraph::new("│").style(theme.ui(UiRole::SurfaceEdge)),
                Rect::new(tree.right(), y, 1, 1),
            );
        }
    }

    let Some(file) = takeover.diff_files.get(takeover.selected_file) else {
        frame.render_widget(
            Paragraph::new("No tracked changes").style(theme.ui(UiRole::Muted)),
            diff,
        );
        return (0, 0, usize::from(diff.height), Vec::new(), file_hits);
    };
    frame.render_widget(
        Paragraph::new(two_sided(
            &file.path,
            &format!("+{} -{}", file.additions, file.deletions),
            diff.width,
        ))
        .style(theme.ui(UiRole::Accent)),
        Rect::new(diff.x, diff.y, diff.width, 1),
    );
    let rows = Rect::new(
        diff.x,
        diff.y.saturating_add(1),
        diff.width,
        diff.height.saturating_sub(1),
    );
    let viewport_rows = usize::from(rows.height);
    let maximum = file.lines.len().saturating_sub(viewport_rows);
    let start = takeover.scroll.min(maximum);
    let mut line_hits = Vec::new();
    for (offset, (index, line)) in file
        .lines
        .iter()
        .enumerate()
        .skip(start)
        .take(viewport_rows)
        .enumerate()
    {
        let y = rows
            .y
            .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX));
        let selected = index == takeover.selected;
        let marker = if selected { "❯" } else { " " };
        let old = line
            .old_line
            .map_or_else(|| "    ".to_string(), |number| format!("{number:>4}"));
        let new = line
            .new_line
            .map_or_else(|| "    ".to_string(), |number| format!("{number:>4}"));
        let sign = match line.kind {
            crate::DiffLineKind::Addition => "+",
            crate::DiffLineKind::Deletion => "-",
            crate::DiffLineKind::Hunk => "@",
            crate::DiffLineKind::Context | crate::DiffLineKind::Metadata => " ",
        };
        let text = truncate_end(
            &format!("{marker}{old} {new} {sign} {}", line.text),
            rows.width,
        );
        let role = if selected && takeover.diff_pane == DiffPane::Content {
            UiRole::Selection
        } else {
            match line.kind {
                crate::DiffLineKind::Addition => UiRole::Success,
                crate::DiffLineKind::Deletion => UiRole::Error,
                crate::DiffLineKind::Hunk => UiRole::Accent,
                crate::DiffLineKind::Context | crate::DiffLineKind::Metadata => UiRole::Foreground,
            }
        };
        frame.render_widget(
            Paragraph::new(text).style(theme.ui(role)),
            Rect::new(rows.x, y, rows.width, 1),
        );
        line_hits.push(TakeoverHit {
            index,
            area: Rect::new(rows.x, y, rows.width, 1),
        });
    }
    if show_tree && takeover.diff_pane == DiffPane::Files {
        (
            file_start,
            takeover.diff_files.len(),
            file_viewport_rows,
            line_hits,
            file_hits,
        )
    } else {
        (start, file.lines.len(), viewport_rows, line_hits, file_hits)
    }
}

fn render_theme_picker(frame: &mut Frame<'_>, frame_area: Rect, app: &AppModel) -> Vec<ThemeHit> {
    let Some(picker) = app.theme_picker() else {
        return Vec::new();
    };
    let width = 62.min(frame_area.width.saturating_sub(4)).max(30);
    let height = 13.min(frame_area.height.saturating_sub(2)).max(10);
    let area = Rect::new(
        frame_area
            .x
            .saturating_add(frame_area.width.saturating_sub(width) / 2),
        frame_area
            .y
            .saturating_add(frame_area.height.saturating_sub(height) / 2),
        width.min(frame_area.width),
        height.min(frame_area.height),
    );
    let theme = theme(app);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" Select a color mode ")
        .style(theme.ui(UiRole::Surface))
        .border_style(theme.ui(UiRole::Border));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return Vec::new();
    }
    frame.render_widget(
        Paragraph::new("Choose LocalPilot's semantic terminal colors.")
            .style(theme.ui(UiRole::Foreground)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    let option_width = 22.min(inner.width);
    let mut hits = Vec::new();
    for (index, option) in Theme::ALL.iter().enumerate() {
        let row = u16::try_from(index).unwrap_or(u16::MAX);
        let y = inner.y.saturating_add(2).saturating_add(row);
        if y >= inner.bottom().saturating_sub(2) {
            break;
        }
        let selected = index == picker.selected;
        let current = *option == picker.original;
        let marker = if selected { "❯" } else { " " };
        let check = if current { " ✓" } else { "" };
        let text = format!("{marker}{}. {}{check}", index + 1, option.display_name());
        let style = if selected {
            theme.ui(UiRole::Focus)
        } else {
            theme.ui(UiRole::Foreground)
        };
        let hit = Rect::new(inner.x, y, option_width, 1);
        frame.render_widget(Paragraph::new(text).style(style), hit);
        hits.push(ThemeHit { index, area: hit });
    }

    let preview_x = inner.x.saturating_add(option_width).saturating_add(1);
    let preview_width = inner.right().saturating_sub(preview_x);
    if preview_width > 0 {
        for (offset, (text, role)) in [
            ("1 - fn previous()", UiRole::Error),
            ("1 + fn improved()", UiRole::Success),
            ("2   return result", UiRole::Code),
            ("3   // selected", UiRole::Focus),
        ]
        .into_iter()
        .enumerate()
        {
            let row = u16::try_from(offset).unwrap_or(u16::MAX);
            let y = inner.y.saturating_add(2).saturating_add(row);
            if y >= inner.bottom().saturating_sub(2) {
                break;
            }
            frame.render_widget(
                Paragraph::new(text).style(theme.ui(role)),
                Rect::new(preview_x, y, preview_width, 1),
            );
        }
    }

    let selected = Theme::ALL
        .get(picker.selected)
        .copied()
        .unwrap_or(Theme::Default);
    frame.render_widget(
        Paragraph::new(theme_description(selected)).style(theme.ui(UiRole::Muted)),
        Rect::new(inner.x, inner.bottom().saturating_sub(2), inner.width, 1),
    );
    frame.render_widget(
        Paragraph::new("↑/↓ preview · Enter select · Esc cancel").style(theme.ui(UiRole::Muted)),
        Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1),
    );
    hits
}

fn theme_description(theme: Theme) -> &'static str {
    match theme {
        Theme::Default => "Balanced colors for dark terminal backgrounds",
        Theme::Dim => "Lower-intensity chrome and conversation colors",
        Theme::HighContrast => "Strong non-color separation and bright focus",
        Theme::Colorblind => "Blue and amber status cues without red/green dependence",
    }
}

fn help_lines(
    takeover: TakeoverView<'_>,
    width: u16,
    theme: ThemeResolver,
    mouse_capture: bool,
) -> Vec<Line<'static>> {
    let mut source = vec![
        ("Conversation commands".to_string(), UiRole::Accent),
        (String::new(), UiRole::Foreground),
    ];
    source.extend(takeover.commands.iter().map(|command| {
        (
            format!("  /{:<10} {}", command.name, command.description),
            UiRole::Foreground,
        )
    }));
    source.extend([
        (String::new(), UiRole::Foreground),
        ("Editing".to_string(), UiRole::Accent),
        (
            "  Enter       Send the current prompt".to_string(),
            UiRole::Foreground,
        ),
        (
            "  Shift+Enter Add a line without sending".to_string(),
            UiRole::Foreground,
        ),
        (
            "  ↑ / ↓       Move through a multiline draft, then prompt history".to_string(),
            UiRole::Foreground,
        ),
        (
            "  Ctrl+R      Search prompt history".to_string(),
            UiRole::Foreground,
        ),
        (
            "  Ctrl+F      Search this conversation from an empty composer".to_string(),
            UiRole::Foreground,
        ),
        (
            "  Ctrl+G      Edit the draft in your configured editor".to_string(),
            UiRole::Foreground,
        ),
        (
            "  !           Enter direct shell-command mode".to_string(),
            UiRole::Foreground,
        ),
        (String::new(), UiRole::Foreground),
        ("Timeline and work".to_string(), UiRole::Accent),
        (
            if mouse_capture {
                "  Wheel/Page  Scroll while keeping the composer active".to_string()
            } else {
                "  Page Up/Down Scroll while keeping the composer active".to_string()
            },
            UiRole::Foreground,
        ),
        (
            if mouse_capture {
                "  Drag        Select timeline text or move the scrollbar".to_string()
            } else {
                "  Mouse       Disabled for this launch".to_string()
            },
            UiRole::Foreground,
        ),
        (
            "  Esc         Close the focused view; during work, stop and steer".to_string(),
            UiRole::Foreground,
        ),
        (
            "  Ctrl+C      Copy a selection; press twice consecutively to exit".to_string(),
            UiRole::Foreground,
        ),
        (String::new(), UiRole::Foreground),
        (
            "LocalPilot keeps provider, permission, and tool behavior unchanged in this view."
                .to_string(),
            UiRole::Muted,
        ),
    ]);

    let mut lines = Vec::new();
    for (text, role) in source {
        for range in crate::text::wrap_ranges(&text, width) {
            lines.push(Line::styled(
                text[range.start_byte..range.end_byte].to_string(),
                theme.ui(role),
            ));
        }
    }
    lines
}

fn draw_scrollbar(frame: &mut Frame<'_>, scrollbar: ScrollbarGeometry, app: &AppModel) {
    let Some(thumb) = scrollbar.thumb else {
        return;
    };
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
            (CompletionKind::CommandValue, _) => "  No matching values",
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
            CompletionKind::CommandValue => item.name.clone(),
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
    draw_scrollbar(frame, scrollbar, app);
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
        ItemKind::Shell => {
            let (glyph, role) = match activity {
                Some(ActivityState::Error) => ("✗ ", UiRole::Error),
                Some(ActivityState::Success) => ("$ ", UiRole::Success),
                Some(ActivityState::Running) | None => ("◉ ", UiRole::Code),
            };
            vec![Span::styled(
                if first { glyph } else { "│ " },
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
    if let Some((search, search_cursor)) = input_overlay_projection(app, width) {
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
    let placeholder = app.editor.text().is_empty() && app.shell_mode();
    let composer_text = if placeholder {
        "Run a shell command"
    } else {
        app.editor.text()
    };
    frame.render_widget(
        Paragraph::new(composer_text.to_string())
            .style(if placeholder {
                surface.patch(theme.ui(UiRole::Muted))
            } else {
                surface
            })
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

fn input_overlay_projection(app: &AppModel, width: u16) -> Option<(String, usize)> {
    timeline_search_projection(app, width).or_else(|| reverse_search_projection(app))
}

fn timeline_search_projection(app: &AppModel, width: u16) -> Option<(String, usize)> {
    let search = app.timeline_search()?;
    let width = width.max(1);
    let mut text = format!("❯ {}", search.query);
    let cursor = text.len();
    let counter = format!("{} / {}", search.current, search.total);
    let counter_width = u16::try_from(UnicodeWidthStr::width(counter.as_str())).unwrap_or(u16::MAX);
    let (_, cursor_column) = crate::editor::text_row_and_column(&text, cursor, width);
    let remaining = width.saturating_sub(cursor_column);
    if counter_width < remaining {
        text.push_str(&" ".repeat(usize::from(remaining.saturating_sub(counter_width))));
    } else {
        text.push('\n');
        text.push_str(&" ".repeat(usize::from(width.saturating_sub(counter_width))));
    }
    text.push_str(&counter);
    Some((text, cursor))
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
    if app.timeline_search().is_some() {
        return "search · ↑↓ navigate · esc close".to_string();
    }
    if app.quick_help() {
        return "quick help · ? or Esc close".to_string();
    }
    if let Some(completion) = app.completion() {
        let fallback = match (completion.kind, completion.loading) {
            (CompletionKind::File, true) => "indexing workspace files",
            (CompletionKind::File, false) => "file completion",
            (CompletionKind::Command, _) => "command completion",
            (CompletionKind::CommandValue, _) => "command value",
        };
        let detail = completion
            .items
            .get(completion.selected)
            .map_or(fallback, |item| item.description.as_str());
        return format!("{detail} · ↑↓ navigate · Enter/Tab accept · Esc close");
    }
    if app.timeline.selected_text().is_some() {
        return if app.capabilities.clipboard_write {
            "selection · Ctrl+C / right-click copy".to_string()
        } else {
            "selection · clipboard unavailable".to_string()
        };
    }
    if app.shell_mode() {
        return "shell mode · Esc exit shell mode".to_string();
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
    fn help_takeover_replaces_chat_chrome_and_owns_its_scrollbar() {
        let mut app = model();
        let _ = app
            .timeline
            .push(ItemKind::Assistant, "HIDDEN_TIMELINE_MARKER");
        app.set_command_catalog([
            crate::CompletionCommand {
                name: "model".into(),
                description: "Switch model".into(),
            },
            crate::CompletionCommand {
                name: "new".into(),
                description: "Start a session".into(),
            },
            crate::CompletionCommand {
                name: "fork".into(),
                description: "Fork a session".into(),
            },
            crate::CompletionCommand {
                name: "clone".into(),
                description: "Clone a session".into(),
            },
            crate::CompletionCommand {
                name: "clear".into(),
                description: "Clear the view".into(),
            },
            crate::CompletionCommand {
                name: "quit".into(),
                description: "Exit".into(),
            },
            crate::CompletionCommand {
                name: "search".into(),
                description: "Search messages".into(),
            },
            crate::CompletionCommand {
                name: "help".into(),
                description: "Open help".into(),
            },
        ]);
        app.open_help();
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("terminal");
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw help");

        let rendered = terminal.backend().to_string();
        let hit_map = hit_map.expect("help hit map");
        assert!(hit_map.takeover);
        assert!(hit_map.frame.is_none());
        assert!(hit_map.tabs.is_empty());
        assert_eq!(hit_map.composer, Rect::default());
        assert!(hit_map.scrollbar.thumb.is_some());
        assert!(rendered.contains("Conversation commands"));
        assert!(rendered.contains("/model"));
        assert!(rendered.contains("Esc close"));
        assert!(!rendered.contains("HIDDEN_TIMELINE_MARKER"));
    }

    #[test]
    fn settings_takeover_renders_focused_values_descriptions_and_mouse_hits() {
        let mut app = model();
        app.open_settings([
            crate::SettingEntry {
                section: "Input".into(),
                name: "Mouse reporting".into(),
                value: "On".into(),
                description: "Capture pointer events".into(),
            },
            crate::SettingEntry {
                section: "Appearance".into(),
                name: "Color mode".into(),
                value: "Default".into(),
                description: "Semantic terminal colors".into(),
            },
        ]);
        app.scroll_takeover_by(1, 2, 10);
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("terminal");
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw settings");

        let rendered = terminal.backend().to_string();
        let hit_map = hit_map.expect("settings hit map");
        assert!(rendered.contains("Settings"));
        assert!(rendered.contains("Appearance · Color mode"));
        assert!(rendered.contains("Semantic terminal colors"));
        assert_eq!(hit_map.takeover_rows.len(), 2);
        assert_eq!(hit_map.takeover_rows[1].index, 1);
    }

    #[test]
    fn diff_takeover_renders_two_panes_semantic_lines_and_hits() {
        let mut app = model();
        app.open_diff([crate::DiffFile {
            status: "M".into(),
            path: "src/main.rs".into(),
            additions: 1,
            deletions: 1,
            lines: vec![
                crate::DiffLine {
                    old_line: None,
                    new_line: None,
                    kind: crate::DiffLineKind::Hunk,
                    text: "@@ -1 +1 @@".into(),
                },
                crate::DiffLine {
                    old_line: Some(1),
                    new_line: None,
                    kind: crate::DiffLineKind::Deletion,
                    text: "old".into(),
                },
                crate::DiffLine {
                    old_line: None,
                    new_line: Some(1),
                    kind: crate::DiffLineKind::Addition,
                    text: "new".into(),
                },
            ],
        }]);
        app.scroll_takeover_by(1, 3, 10);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw diff");

        let rendered = terminal.backend().to_string();
        let hit_map = hit_map.expect("diff hit map");
        assert!(rendered.contains("Files (1)"));
        assert!(rendered.matches("src/main.rs").count() >= 2);
        assert!(rendered.contains("@@ -1 +1 @@"));
        assert!(rendered.contains("old"));
        assert!(rendered.contains("new"));
        assert_eq!(hit_map.takeover_file_rows.len(), 1);
        assert_eq!(hit_map.takeover_rows.len(), 3);
    }

    #[test]
    fn diff_file_pane_keeps_large_trees_visible_and_owns_the_scrollbar() {
        let mut app = model();
        app.open_diff((0..20).map(|index| crate::DiffFile {
            status: "M".into(),
            path: format!("src/file-{index:02}.rs"),
            additions: 1,
            deletions: 0,
            lines: vec![crate::DiffLine {
                old_line: None,
                new_line: Some(1),
                kind: crate::DiffLineKind::Addition,
                text: "new".into(),
            }],
        }));
        let _ = app.handle_input(crate::InputAction::MoveLeft, 76);
        app.scroll_takeover_to(10, 20, 8);

        let mut terminal = Terminal::new(TestBackend::new(80, 12)).expect("terminal");
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw diff tree");

        let rendered = terminal.backend().to_string();
        let hit_map = hit_map.expect("diff tree hit map");
        assert!(rendered.contains("src/file-10.rs"));
        assert!(!rendered.contains("src/file-00.rs"));
        assert_eq!(hit_map.takeover_file_rows[0].index, 10);
        assert_eq!(hit_map.scrollbar.total_rows, 20);
        assert!(hit_map.scrollbar.thumb.is_some());
    }

    #[test]
    fn quick_help_is_a_two_column_timeline_overlay() {
        let mut app = model();
        for number in 0..20 {
            let _ = app
                .timeline
                .push(ItemKind::Assistant, format!("timeline row {number}"));
        }
        let _ = app.handle_input(crate::InputAction::Insert("?".to_string()), 76);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw quick help");

        let rendered = terminal.backend().to_string();
        let hit_map = hit_map.expect("hit map");
        assert!(!hit_map.takeover);
        assert!(hit_map.frame.is_some());
        assert!(rendered.contains("Quick help"));
        assert!(rendered.contains("Enter       send prompt"));
        assert!(rendered.contains("Page Up/Down scroll timeline"));
        assert!(footer_state(&app).contains("? or Esc close"));
        assert!(hit_map.timeline_rows.len() < usize::from(hit_map.timeline.height));
    }

    #[test]
    fn theme_picker_is_centered_numbered_and_exposes_mouse_hits() {
        let mut app = model();
        app.open_theme_picker();
        let _ = app.handle_input(crate::InputAction::MoveDown, 76);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw theme picker");

        let rendered = terminal.backend().to_string();
        let hit_map = hit_map.expect("hit map");
        assert_eq!(hit_map.theme_rows.len(), Theme::ALL.len());
        assert!(rendered.contains("Select a color mode"));
        assert!(rendered.contains("1. Default ✓"));
        assert!(rendered.contains("❯2. Dim"));
        assert!(rendered.contains("1 - fn previous()"));
        assert!(rendered.contains("Enter select · Esc cancel"));
        assert_eq!(app.theme, Theme::Dim);
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
    fn timeline_search_replaces_composer_with_query_and_right_aligned_count() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = model();
        let _ = app.timeline.push(ItemKind::User, "marker marker");
        let _ = app.timeline.push(ItemKind::Assistant, "new MARKER");
        let _ = app.handle_input(crate::InputAction::Insert("/search marker".to_string()), 76);
        let _ = app.handle_input(crate::InputAction::Submit, 76);
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw timeline search");
        let layout = hit_map.expect("hit map").frame.expect("frame layout");
        let buffer = terminal.backend().buffer();
        let composer = (layout.composer_content.x..layout.composer_content.right())
            .filter_map(|x| buffer.cell((x, layout.composer_content.y)))
            .map(ratatui::buffer::Cell::symbol)
            .collect::<Vec<_>>()
            .join("");
        assert!(composer.starts_with("❯ marker"));
        assert!(composer.ends_with("2 / 2"));
        assert_eq!(footer_state(&app), "search · ↑↓ navigate · esc close");
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
    fn command_value_picker_reuses_completion_geometry_without_a_slash_prefix() {
        let mut app = model();
        app.set_command_catalog([crate::CompletionCommand {
            name: "model".to_string(),
            description: "Switch provider or model".to_string(),
        }]);
        app.set_command_values(
            "model",
            [crate::CompletionCommand {
                name: "local".to_string(),
                description: "current provider".to_string(),
            }],
        );
        let _ = app.handle_input(crate::InputAction::Insert("/mo".to_string()), 76);
        let _ = app.handle_input(crate::InputAction::AcceptCompletion, 76);

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw value picker");
        let hit_map = hit_map.expect("hit map");
        assert_eq!(hit_map.completion_rows.len(), 1);
        let selected = buffer_line(
            terminal.backend().buffer(),
            hit_map.completion_rows[0].area.y,
        );
        assert!(selected.contains("❯ local"));
        assert!(!selected.contains("/local"));
        assert!(selected.contains("current provider"));
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
    fn selected_text_footer_exposes_available_copy_action() {
        let mut app = model();
        let item = app
            .timeline
            .push(ItemKind::Assistant, "copy this")
            .expect("timeline item");
        app.timeline.start_selection(crate::ContentPoint {
            item_id: item,
            byte: 0,
        });
        app.timeline.extend_selection(crate::ContentPoint {
            item_id: item,
            byte: 4,
        });

        assert_eq!(footer_state(&app), "selection · clipboard unavailable");
        app.capabilities.clipboard_write = true;
        assert_eq!(footer_state(&app), "selection · Ctrl+C / right-click copy");
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

    #[test]
    fn leading_bang_owns_the_shell_mode_footer_until_escape() {
        let mut app = model();
        let _ = app.handle_input(crate::InputAction::Insert("!echo marker".to_string()), 76);
        assert_eq!(app.editor.text(), "echo marker");
        assert_eq!(footer_state(&app), "shell mode · Esc exit shell mode");
        let _ = app.handle_input(crate::InputAction::Escape, 76);
        assert_eq!(app.editor.text(), "echo marker");
        assert_eq!(footer_state(&app), "idle · Ctrl+C copy / twice to exit");
    }

    #[test]
    fn lone_bang_is_consumed_into_shell_mode_and_renders_its_placeholder() {
        let mut app = model();
        let _ = app.handle_input(crate::InputAction::Insert("!".to_string()), 76);
        assert!(app.shell_mode());
        assert!(app.editor.text().is_empty());

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("render shell placeholder");
        assert!(terminal
            .backend()
            .to_string()
            .contains("Run a shell command"));
    }

    #[test]
    fn shell_results_render_target_status_glyphs_line_counts_and_output_gutters() {
        let mut app = model();
        let _ = app.handle_input(crate::InputAction::Insert("!echo marker".to_string()), 76);
        let crate::AppCommand::RunShell(success_command) =
            app.handle_input(crate::InputAction::Submit, 76)
        else {
            panic!("success shell command");
        };
        let success = app
            .append_shell(&success_command, false)
            .expect("success item");
        let success_output = crate::UserShellOutput::captured(0, "one\ntwo\n", "");
        assert!(app.finish_shell(success, &success_command, &success_output));

        let _ = app.handle_input(crate::InputAction::Insert("!bad-command".to_string()), 76);
        let crate::AppCommand::RunShell(failure_command) =
            app.handle_input(crate::InputAction::Submit, 76)
        else {
            panic!("failure shell command");
        };
        let failure = app
            .append_shell(&failure_command, false)
            .expect("failure item");
        let failure_output = crate::UserShellOutput::captured(5, "", "diagnostic\n");
        assert!(app.finish_shell(failure, &failure_command, &failure_output));

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("render shell results");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("$ Shell echo marker 2 lines"));
        assert!(rendered.contains("│ one"));
        assert!(rendered.contains("│ two"));
        assert!(rendered.contains("✗ Shell bad-command 1 line"));
        assert!(rendered.contains("│ diagnostic"));
        assert!(!rendered.contains("exit 5"));
    }
}
