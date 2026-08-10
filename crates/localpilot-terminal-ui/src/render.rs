use std::time::Duration;

use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Widget, Wrap};
use ratatui::Frame;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::{
    sanitize_inline, ActiveBody, CompletionKind, DiffPane, LocalMindSection, QuestionView,
    TakeoverView, TrustView,
};
use crate::layout::tab_height;
use crate::projection::{PeerPane, SessionProjection};
use crate::{
    ActivityState, AppModel, ColorSupport, DialogState, Focus, FrameLayout, ItemKind, PinnedPrompt,
    TabId, TakeoverKind, TextStyle, Theme, ThemeResolver, TimelineLayout, TimelinePaneLayout,
    UiRole, VisualRow, VisualRowPart, APP_NAME, MINIMUM_HEIGHT, MINIMUM_WIDTH,
};

/// Six banner lines plus one deliberate blank line before the first prompt.
const BANNER_ROWS: u16 = 7;
/// Width of the widest screen-reader timeline role label (`Shell completed: `).
const SCREEN_READER_PREFIX_EXTRA: u16 = 17;
const TRUST_OPTIONS: [&str; 3] = ["Session only", "Trust and remember", "No - exit"];

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
pub struct QuestionHit {
    pub index: usize,
    pub area: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrustHit {
    pub index: usize,
    pub area: Rect,
}

#[derive(Clone, PartialEq, Eq)]
pub struct TrustPathHit {
    pub area: Rect,
    text: String,
}

impl std::fmt::Debug for TrustPathHit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TrustPathHit")
            .field("area", &self.area)
            .field(
                "text",
                &format_args!("<{} bytes redacted>", self.text.len()),
            )
            .finish()
    }
}

impl TrustPathHit {
    #[must_use]
    pub fn byte_for_column(&self, column: u16, trailing: bool) -> usize {
        let target = usize::from(column.saturating_sub(self.area.x));
        let mut display_column = 0usize;
        for (byte, grapheme) in self.text.grapheme_indices(true) {
            let width = UnicodeWidthStr::width(grapheme).max(1);
            if target < display_column.saturating_add(width) {
                let past_midpoint = target.saturating_sub(display_column) >= width.div_ceil(2);
                return if trailing && past_midpoint {
                    byte.saturating_add(grapheme.len())
                } else {
                    byte
                };
            }
            display_column = display_column.saturating_add(width);
        }
        self.text.len()
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

#[derive(Debug, Default)]
struct TrustRenderHits {
    rows: Vec<TrustHit>,
    path: Option<TrustPathHit>,
}

#[derive(Debug, Default)]
struct DialogHits {
    question_rows: Vec<QuestionHit>,
    trust_rows: Vec<TrustHit>,
    trust_path: Option<TrustPathHit>,
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

/// Hit-test and viewport geometry for one visible session timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelinePaneHits {
    pub peer: Option<PeerPane>,
    pub label: Option<Rect>,
    pub viewport: Rect,
    pub timeline: Rect,
    pub wrap_width: u16,
    pub rows: Vec<TimelineRowHit>,
    pub scrollbar: ScrollbarGeometry,
}

impl TimelinePaneHits {
    #[must_use]
    pub const fn contains(&self, column: u16, row: u16) -> bool {
        let in_viewport = column >= self.viewport.x
            && column < self.viewport.right()
            && row >= self.viewport.y
            && row < self.viewport.bottom();
        let in_label = match self.label {
            Some(label) => {
                column >= label.x
                    && column < label.right()
                    && row >= label.y
                    && row < label.bottom()
            }
            None => false,
        };
        in_viewport || in_label
    }
}

/// Visible timeline hit regions for an ordinary, fallback, or split frame.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TimelineHits {
    Single(TimelinePaneHits),
    Pair {
        a: TimelinePaneHits,
        divider: Rect,
        b: TimelinePaneHits,
    },
}

impl TimelineHits {
    #[must_use]
    pub fn active(&self, active_peer: Option<PeerPane>) -> Option<&TimelinePaneHits> {
        match self {
            Self::Single(pane)
                if matches!(
                    (pane.peer, active_peer),
                    (None, None)
                        | (Some(PeerPane::A), Some(PeerPane::A))
                        | (Some(PeerPane::B), Some(PeerPane::B))
                ) =>
            {
                Some(pane)
            }
            Self::Single(_) => None,
            Self::Pair { a, b, .. } => match active_peer {
                Some(PeerPane::A) => Some(a),
                Some(PeerPane::B) => Some(b),
                None => None,
            },
        }
    }

    fn active_mut(&mut self, active_peer: Option<PeerPane>) -> Option<&mut TimelinePaneHits> {
        match self {
            Self::Single(pane)
                if matches!(
                    (pane.peer, active_peer),
                    (None, None)
                        | (Some(PeerPane::A), Some(PeerPane::A))
                        | (Some(PeerPane::B), Some(PeerPane::B))
                ) =>
            {
                Some(pane)
            }
            Self::Single(_) => None,
            Self::Pair { a, b, .. } => match active_peer {
                Some(PeerPane::A) => Some(a),
                Some(PeerPane::B) => Some(b),
                None => None,
            },
        }
    }

    #[must_use]
    pub fn for_peer(&self, peer: PeerPane) -> Option<&TimelinePaneHits> {
        match self {
            Self::Single(pane) => (pane.peer == Some(peer)).then_some(pane),
            Self::Pair { a, b, .. } => Some(match peer {
                PeerPane::A => a,
                PeerPane::B => b,
            }),
        }
    }

    #[must_use]
    pub fn at(&self, column: u16, row: u16) -> Option<&TimelinePaneHits> {
        match self {
            Self::Single(pane) => pane.contains(column, row).then_some(pane),
            Self::Pair { a, b, .. } => {
                if a.contains(column, row) {
                    Some(a)
                } else {
                    b.contains(column, row).then_some(b)
                }
            }
        }
    }

    #[must_use]
    pub const fn divider(&self) -> Option<Rect> {
        match self {
            Self::Single(_) => None,
            Self::Pair { divider, .. } => Some(*divider),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitMap {
    pub takeover: bool,
    pub takeover_content: Rect,
    pub frame: Option<FrameLayout>,
    pub tabs: Vec<TabHit>,
    pub timelines: Option<TimelineHits>,
    pub completion_rows: Vec<CompletionHit>,
    pub theme_rows: Vec<ThemeHit>,
    pub question_rows: Vec<QuestionHit>,
    pub trust_rows: Vec<TrustHit>,
    pub trust_path: Option<TrustPathHit>,
    pub takeover_rows: Vec<TakeoverHit>,
    pub takeover_file_rows: Vec<TakeoverHit>,
    pub takeover_scrollbar: ScrollbarGeometry,
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
    match app.active_body() {
        ActiveBody::Takeover => {
            if let Some(takeover) = app.takeover() {
                return render_takeover(frame, area, app, takeover);
            }
        }
        ActiveBody::LocalMind => {
            if let Some(localmind) = app.localmind_tab() {
                let tabs_height =
                    tab_height(area.width, app.capabilities.screen_reader).min(area.height);
                let tabs_area = Rect::new(area.x, area.y, area.width, tabs_height);
                let body = Rect::new(
                    area.x,
                    area.y.saturating_add(tabs_height),
                    area.width,
                    area.height.saturating_sub(tabs_height),
                );
                let tabs = render_tabs(frame, tabs_area, app);
                let mut hit_map = render_takeover(frame, body, app, localmind);
                hit_map.tabs = tabs;
                hit_map.theme_rows = render_theme_picker(frame, area, app);
                return hit_map;
            }
        }
        ActiveBody::Session => {}
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
    let layout = if app.is_pair() {
        FrameLayout::calculate_pair_for_mode(
            area,
            requested_editor_rows,
            app.capabilities.screen_reader,
        )
    } else {
        FrameLayout::calculate_for_mode(area, requested_editor_rows, app.capabilities.screen_reader)
    };
    let Some(layout) = layout else {
        frame.render_widget(
            Paragraph::new(format!(
                "{APP_NAME}\nresize to at least {MINIMUM_WIDTH} × {MINIMUM_HEIGHT}"
            ))
            .wrap(Wrap { trim: false }),
            area,
        );
        return HitMap {
            takeover: false,
            takeover_content: Rect::default(),
            frame: None,
            tabs: Vec::new(),
            timelines: None,
            completion_rows: Vec::new(),
            theme_rows: Vec::new(),
            question_rows: Vec::new(),
            trust_rows: Vec::new(),
            trust_path: None,
            takeover_rows: Vec::new(),
            takeover_file_rows: Vec::new(),
            takeover_scrollbar: ScrollbarGeometry::calculate(Rect::default(), 0, 0, 0),
            composer: Rect::default(),
            editor_width: 1,
            composer_scroll: 0,
        };
    };

    let tabs = render_tabs(frame, layout.tabs, app);
    let active_peer = app.active_pair_pane();
    let mut timelines = match layout.timeline {
        TimelineLayout::Single(timeline) => {
            let projection = app.active_projection();
            render_peer_label(frame, timeline, active_peer, projection, app);
            Some(TimelineHits::Single(render_timeline(
                frame,
                timeline,
                active_peer,
                projection,
                app,
            )))
        }
        TimelineLayout::Pair(pair) => {
            let (Some(a_projection), Some(b_projection)) =
                (app.projection(PeerPane::A), app.projection(PeerPane::B))
            else {
                return HitMap {
                    takeover: false,
                    takeover_content: Rect::default(),
                    frame: Some(layout),
                    tabs,
                    timelines: None,
                    completion_rows: Vec::new(),
                    theme_rows: Vec::new(),
                    question_rows: Vec::new(),
                    trust_rows: Vec::new(),
                    trust_path: None,
                    takeover_rows: Vec::new(),
                    takeover_file_rows: Vec::new(),
                    takeover_scrollbar: ScrollbarGeometry::calculate(Rect::default(), 0, 0, 0),
                    composer: layout.composer_content,
                    editor_width: 1,
                    composer_scroll: 0,
                };
            };
            render_peer_label(frame, pair.first, Some(PeerPane::A), a_projection, app);
            render_peer_label(frame, pair.second, Some(PeerPane::B), b_projection, app);
            render_peer_divider(frame, pair.divider, app);
            Some(TimelineHits::Pair {
                a: render_timeline(frame, pair.first, Some(PeerPane::A), a_projection, app),
                divider: pair.divider,
                b: render_timeline(frame, pair.second, Some(PeerPane::B), b_projection, app),
            })
        }
    };
    let active_timeline = timelines
        .as_ref()
        .and_then(|timelines| timelines.active(active_peer))
        .map_or(Rect::default(), |hits| hits.timeline);
    let quick_help_area = render_quick_help(frame, active_timeline, app);
    let (completion_area, completion_rows) = render_completion(frame, active_timeline, app);
    if let Some(overlay_area) = completion_area.or(quick_help_area) {
        if let Some(active) = timelines
            .as_mut()
            .and_then(|timelines| timelines.active_mut(active_peer))
        {
            active.rows.retain(|hit| hit.y < overlay_area.y);
        }
    }
    render_status(frame, layout.status, app, layout.stacked);
    let (editor_width, composer_scroll) = render_composer(frame, layout, app);
    render_footer(frame, layout.footer, app, layout.stacked);
    let theme_rows = render_theme_picker(frame, area, app);
    let dialog_hits = render_dialog(frame, area, active_timeline, app);
    HitMap {
        takeover: false,
        takeover_content: Rect::default(),
        frame: Some(layout),
        tabs,
        timelines,
        completion_rows,
        theme_rows,
        question_rows: dialog_hits.question_rows,
        trust_rows: dialog_hits.trust_rows,
        trust_path: dialog_hits.trust_path,
        takeover_rows: Vec::new(),
        takeover_file_rows: Vec::new(),
        takeover_scrollbar: ScrollbarGeometry::calculate(Rect::default(), 0, 0, 0),
        composer: layout.composer_content,
        editor_width,
        composer_scroll,
    }
}

fn render_quick_help(frame: &mut Frame<'_>, area: Rect, app: &AppModel) -> Option<Rect> {
    if !app.quick_help() || area.height == 0 {
        return None;
    }
    // Image attach is a single-session affordance; a pair session rejects it, so
    // the shared help advertises it only outside pair mode.
    let (image_left, image_right) = if app.is_pair() {
        ("images unavailable in pair", "")
    } else {
        ("Ctrl+V      paste an image", "  vision: bitmap/file/path")
    };
    let left = [
        "Enter       send prompt",
        "Shift+Enter add a line",
        "↑ / ↓       edit, then history",
        "Ctrl+R      search history",
        "Ctrl+G      external editor",
        "Ctrl+S      stash / restore draft",
        image_left,
    ];
    let right = if app.capabilities.mouse_capture {
        [
            "Page Up/Down scroll timeline",
            "Wheel        scroll timeline",
            "Drag / icon  select / expand",
            "Ctrl+F       search messages",
            "Esc          stop and steer",
            "Esc Esc      clear draft",
            image_right,
        ]
    } else {
        [
            "Page Up/Down scroll timeline",
            "Mouse        disabled",
            "Ctrl+C       copy / clear / cancel / exit",
            "Ctrl+F       search messages",
            "Esc          stop and steer",
            "Esc Esc      clear draft",
            image_right,
        ]
    };
    let wide = area.width >= 70;
    let pair_rows = u16::from(app.is_pair());
    let requested = if wide {
        8_u16.saturating_add(pair_rows)
    } else {
        15_u16.saturating_add(pair_rows)
    };
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
        if app.is_pair() {
            let y = help.y.saturating_add(8);
            if y < help.bottom() {
                frame.render_widget(
                    Paragraph::new("F6          switch peer").style(theme.ui(UiRole::Foreground)),
                    Rect::new(help.x.saturating_add(2), y, column_width, 1),
                );
            }
        }
    } else {
        for (index, text) in left
            .iter()
            .copied()
            .chain(app.is_pair().then_some("F6          switch peer"))
            .chain(right.iter().copied())
            .enumerate()
        {
            let row = u16::try_from(index).unwrap_or(u16::MAX);
            let y = help.y.saturating_add(1).saturating_add(row);
            if y >= help.bottom() {
                break;
            }
            frame.render_widget(
                Paragraph::new(text).style(theme.ui(UiRole::Foreground)),
                Rect::new(help.x.saturating_add(2), y, help.width.saturating_sub(4), 1),
            );
        }
    }
    Some(help)
}

fn settings_indices(takeover: TakeoverView<'_>) -> Vec<usize> {
    let query = takeover.settings_query.trim().to_lowercase();
    takeover
        .settings
        .iter()
        .enumerate()
        .filter_map(|(index, setting)| {
            (query.is_empty()
                || setting.name.to_lowercase().contains(&query)
                || setting.section.to_lowercase().contains(&query))
            .then_some(index)
        })
        .collect()
}

fn settings_footer(takeover: TakeoverView<'_>, indices: &[usize], width: u16) -> String {
    let selected = indices
        .get(takeover.selected)
        .and_then(|index| takeover.settings.get(*index));
    let exit = if takeover.settings_query.is_empty() {
        "Esc close"
    } else {
        "Esc clear"
    };
    let compact = width < 50;
    match selected {
        Some(setting) if setting.edit.is_some() && !setting.is_default && compact => {
            format!("Enter edit · Ctrl+R reset · {exit}")
        }
        Some(setting) if setting.edit.is_some() && !setting.is_default => {
            format!("Type search · ↑/↓ select · Enter edit · Ctrl+R reset · {exit}")
        }
        Some(setting) if setting.edit.is_some() && compact => {
            format!("Enter edit · {exit}")
        }
        Some(setting) if setting.edit.is_some() => {
            format!("Type search · ↑/↓ select · Enter edit · {exit}")
        }
        _ if compact => format!("↑/↓ select · {exit}"),
        _ => format!("Type search · ↑/↓ select · Page Up/Page Down · {exit}"),
    }
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
        let dialog_hits = render_dialog(frame, area, area, app);
        return HitMap {
            takeover: true,
            takeover_content: Rect::default(),
            frame: None,
            tabs: Vec::new(),
            timelines: None,
            completion_rows: Vec::new(),
            theme_rows: Vec::new(),
            question_rows: dialog_hits.question_rows,
            trust_rows: dialog_hits.trust_rows,
            trust_path: dialog_hits.trust_path,
            takeover_rows: Vec::new(),
            takeover_file_rows: Vec::new(),
            takeover_scrollbar: ScrollbarGeometry::calculate(Rect::default(), 0, 0, 0),
            composer: Rect::default(),
            editor_width: 1,
            composer_scroll: 0,
        };
    }

    let theme = theme(app);
    let title: Option<String> = match takeover.kind {
        TakeoverKind::Help => Some(" Help ".to_string()),
        TakeoverKind::Sessions => Some(" Sessions ".to_string()),
        TakeoverKind::Settings => Some(" Settings ".to_string()),
        TakeoverKind::Diff => Some(" Diff ".to_string()),
        // Clip the bounded report title to the title bar width so a long title
        // cannot overflow at narrow widths.
        TakeoverKind::Report => {
            // Clip the sanitized report title by DISPLAY WIDTH (not scalar count)
            // so a wide/CJK grapheme cannot overflow the title bar at narrow widths.
            let budget = usize::from(area.width).saturating_sub(4).max(1);
            let mut clipped = String::new();
            let mut used = 0usize;
            for ch in takeover.report_title.chars() {
                let width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if used + width > budget {
                    break;
                }
                used += width;
                clipped.push(ch);
            }
            Some(format!(" {clipped} "))
        }
        // The persistent product-tab strip already supplies this title.
        TakeoverKind::LocalMind => None,
    };
    if let Some(title) = title {
        frame.render_widget(
            Paragraph::new(title.as_str()).style(theme.ui(UiRole::TabActive)),
            Rect::new(
                area.x.saturating_add(1),
                area.y,
                u16::try_from(title.width()).unwrap_or(area.width),
                1,
            ),
        );
    }

    let setting_indices = settings_indices(takeover);
    if takeover.kind == TakeoverKind::Settings {
        let count = setting_indices.len();
        let label = if count == 1 {
            "1 item".to_string()
        } else {
            format!("{count} items")
        };
        let width = u16::try_from(label.width()).unwrap_or(area.width);
        frame.render_widget(
            Paragraph::new(label).style(theme.ui(UiRole::Muted)),
            Rect::new(
                area.right().saturating_sub(width).saturating_sub(2),
                area.y,
                width.min(area.width),
                1,
            ),
        );
    }

    if let Some(localmind) = takeover.localmind {
        let mut x = area.x.saturating_add(2);
        for section in LocalMindSection::ALL {
            let label = format!(" {} ", section.label());
            let width = u16::try_from(label.width()).unwrap_or(area.width);
            if x.saturating_add(width) >= area.right().saturating_sub(1) {
                break;
            }
            let role = if section == localmind.section {
                UiRole::TabActive
            } else {
                UiRole::Muted
            };
            frame.render_widget(
                Paragraph::new(label).style(theme.ui(role)),
                Rect::new(x, area.y, width, 1),
            );
            x = x.saturating_add(width);
        }
    }

    let footer_height = match takeover.kind {
        TakeoverKind::Settings => 3,
        TakeoverKind::Sessions => 2,
        TakeoverKind::Diff | TakeoverKind::Help | TakeoverKind::Report => 1,
        TakeoverKind::LocalMind => 3,
    };
    let content_offset = 2;
    let content = Rect::new(
        area.x.saturating_add(2),
        area.y.saturating_add(content_offset),
        area.width.saturating_sub(5),
        area.height.saturating_sub(content_offset + footer_height),
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
                app.is_pair(),
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
        TakeoverKind::Report => {
            // A plain scrollable list of the bounded report lines, wrapped and
            // styled through the shared takeover line helper (same as Help). The
            // body is the content the user opened the takeover to read, so it is
            // Foreground (Muted is reserved for secondary chrome).
            let (start, total, lines) = text_takeover_window(
                takeover.report_lines,
                takeover.scroll,
                viewport_rows,
                content.width,
                theme,
            );
            frame.render_widget(Paragraph::new(lines), content);
            (start, total, viewport_rows, Vec::new(), Vec::new())
        }
        TakeoverKind::LocalMind => match takeover.localmind {
            Some(localmind) => {
                if localmind.section == LocalMindSection::Review {
                    let maximum = localmind.review.len().saturating_sub(viewport_rows);
                    let start = takeover.scroll.min(maximum);
                    let mut hits = Vec::new();
                    for (offset, (index, row)) in localmind
                        .review
                        .iter()
                        .enumerate()
                        .skip(start)
                        .take(viewport_rows)
                        .enumerate()
                    {
                        let y = content
                            .y
                            .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX));
                        let marker = if index == takeover.selected {
                            "▶"
                        } else {
                            " "
                        };
                        let promoted = if row.promoted { " · promoted" } else { "" };
                        let edit = if row.requires_edit {
                            " · edit required"
                        } else {
                            ""
                        };
                        let text = truncate_end(
                            &format!(
                                "{marker} [{}] {} — {} · {}{}{}",
                                row.state,
                                row.summary,
                                row.category,
                                row.confidence,
                                promoted,
                                edit
                            ),
                            content.width,
                        );
                        let role = if index == takeover.selected {
                            UiRole::Focus
                        } else {
                            UiRole::Foreground
                        };
                        frame.render_widget(
                            Paragraph::new(text).style(theme.ui(role)),
                            Rect::new(content.x, y, content.width, 1),
                        );
                        hits.push(TakeoverHit {
                            index,
                            area: Rect::new(content.x, y, content.width, 1),
                        });
                    }
                    if localmind.review.is_empty() {
                        frame.render_widget(
                            Paragraph::new("No review candidates yet.")
                                .style(theme.ui(UiRole::Muted)),
                            content,
                        );
                    }
                    (
                        start,
                        localmind.review.len(),
                        viewport_rows,
                        hits,
                        Vec::new(),
                    )
                } else {
                    let (start, total, lines) = text_takeover_window(
                        localmind.lines,
                        takeover.scroll,
                        viewport_rows,
                        content.width,
                        theme,
                    );
                    frame.render_widget(Paragraph::new(lines), content);
                    (start, total, viewport_rows, Vec::new(), Vec::new())
                }
            }
            None => (0, 0, viewport_rows, Vec::new(), Vec::new()),
        },
        TakeoverKind::Settings => {
            let section_rows = setting_indices
                .iter()
                .filter_map(|index| takeover.settings.get(*index))
                .map(|setting| setting.section.as_str())
                .fold((0usize, None), |(count, previous), section| {
                    (
                        count + usize::from(previous != Some(section)),
                        Some(section),
                    )
                })
                .0;
            // Section headers are only used when the complete filtered list fits.
            // Otherwise every setting remains a one-row scroll target.
            let compact = area.width < 50
                || setting_indices.len().saturating_add(section_rows) > viewport_rows;
            let maximum = setting_indices.len().saturating_sub(viewport_rows);
            let start = takeover.scroll.min(maximum);
            let mut hits = Vec::new();
            let mut y = content.y;
            let mut previous_section: Option<&str> = None;
            if setting_indices.is_empty() {
                frame.render_widget(
                    Paragraph::new("No settings match this search").style(theme.ui(UiRole::Muted)),
                    content,
                );
            }
            for (filtered_index, setting_index) in
                setting_indices.iter().copied().enumerate().skip(start)
            {
                let setting = &takeover.settings[setting_index];
                if !compact && previous_section != Some(setting.section.as_str()) {
                    if y >= content.bottom() {
                        break;
                    }
                    frame.render_widget(
                        Paragraph::new(setting.section.clone()).style(theme.ui(UiRole::Accent)),
                        Rect::new(content.x, y, content.width, 1),
                    );
                    y = y.saturating_add(1);
                    previous_section = Some(&setting.section);
                }
                if y >= content.bottom() {
                    break;
                }
                let selected = filtered_index == takeover.selected;
                let prefix = if compact && content.width < 32 {
                    ""
                } else if selected {
                    "❯ "
                } else {
                    "  "
                };
                let left = if compact {
                    format!("{prefix}{} · {}", setting.section, setting.name)
                } else {
                    format!("{prefix}{}", setting.name)
                };
                let line = if compact
                    && left
                        .width()
                        .saturating_add(setting.value.width())
                        .saturating_add(1)
                        > usize::from(content.width)
                {
                    truncate_end(&left, content.width)
                } else {
                    two_sided(&left, &setting.value, content.width)
                };
                frame.render_widget(
                    Paragraph::new(line).style(theme.ui(if selected {
                        UiRole::TabActive
                    } else {
                        UiRole::Foreground
                    })),
                    Rect::new(content.x, y, content.width, 1),
                );
                hits.push(TakeoverHit {
                    index: filtered_index,
                    area: Rect::new(content.x, y, content.width, 1),
                });
                y = y.saturating_add(1);
            }
            if let Some(selected) = setting_indices
                .get(takeover.selected)
                .and_then(|index| takeover.settings.get(*index))
            {
                let description = if app.capabilities.screen_reader {
                    format!(
                        "Current selection: {} = {}; {}",
                        selected.name, selected.value, selected.description
                    )
                } else {
                    selected.description.clone()
                };
                frame.render_widget(
                    Paragraph::new(truncate_end(&description, area.width.saturating_sub(3)))
                        .style(theme.ui(UiRole::Muted)),
                    Rect::new(
                        area.x.saturating_add(1),
                        area.bottom().saturating_sub(3),
                        area.width.saturating_sub(3),
                        1,
                    ),
                );
            }
            (
                start,
                setting_indices.len(),
                viewport_rows,
                hits,
                Vec::new(),
            )
        }
        TakeoverKind::Sessions => {
            let maximum = takeover.sessions.len().saturating_sub(viewport_rows);
            let start = takeover.scroll.min(maximum);
            let mut hits = Vec::new();
            if takeover.sessions.is_empty() {
                frame.render_widget(
                    Paragraph::new("No saved sessions in this workspace")
                        .style(theme.ui(UiRole::Muted)),
                    content,
                );
            }
            for (offset, (index, session)) in takeover
                .sessions
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
                let label = session.name.as_deref().unwrap_or(&session.selector);
                let current = if session.current { " · current" } else { "" };
                let left = if session.name.is_some() && content.width >= 54 {
                    format!("{prefix}{label} · {}{current}", session.selector)
                } else {
                    format!("{prefix}{label}{current}")
                };
                let count = if session.message_count == 1 {
                    "1 message".to_string()
                } else {
                    format!("{} messages", session.message_count)
                };
                let right = session
                    .updated
                    .as_deref()
                    .filter(|_| content.width >= 48)
                    .map_or(count.clone(), |updated| format!("{count} · {updated}"));
                frame.render_widget(
                    Paragraph::new(two_sided(&left, &right, content.width)).style(theme.ui(
                        if selected {
                            UiRole::TabActive
                        } else {
                            UiRole::Foreground
                        },
                    )),
                    Rect::new(content.x, y, content.width, 1),
                );
                hits.push(TakeoverHit {
                    index,
                    area: Rect::new(content.x, y, content.width, 1),
                });
            }
            if let Some(selected) = takeover.sessions.get(takeover.selected) {
                let label = selected.name.as_deref().unwrap_or(&selected.selector);
                let current = if selected.current {
                    "; current session"
                } else {
                    ""
                };
                frame.render_widget(
                    Paragraph::new(truncate_end(
                        &format!(
                            "Current selection: {label}; id {}{current}",
                            selected.selector
                        ),
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
                takeover.sessions.len(),
                viewport_rows,
                hits,
                Vec::new(),
            )
        }
        TakeoverKind::Diff => render_diff_takeover(frame, content, app, takeover),
    };

    let scrollbar = ScrollbarGeometry::calculate(
        if app.capabilities.screen_reader {
            Rect::default()
        } else {
            Rect::new(
                area.right().saturating_sub(2),
                area.y.saturating_add(1),
                1,
                area.height.saturating_sub(3),
            )
        },
        start,
        total_rows,
        scrollbar_rows,
    );
    draw_scrollbar(frame, scrollbar, app);
    if takeover.kind == TakeoverKind::Settings {
        let search = if takeover.settings_query.is_empty() {
            "Search: type to filter".to_string()
        } else {
            format!("Search: {}", takeover.settings_query)
        };
        frame.render_widget(
            Paragraph::new(truncate_end(&search, area.width.saturating_sub(3)))
                .style(theme.ui(UiRole::Foreground)),
            Rect::new(
                area.x.saturating_add(1),
                area.bottom().saturating_sub(2),
                area.width.saturating_sub(3),
                1,
            ),
        );
    }
    if let Some(localmind) = takeover.localmind {
        let reviewer = if localmind.editing_reviewer {
            format!(
                "Reviewer: {}_ · Enter save · Esc cancel",
                localmind.reviewer
            )
        } else if localmind.reviewer.is_empty() {
            "Reviewer: not set · i set identity".to_string()
        } else {
            format!("Reviewer: {} · i edit", localmind.reviewer)
        };
        frame.render_widget(
            Paragraph::new(truncate_end(&reviewer, area.width.saturating_sub(3)))
                .style(theme.ui(UiRole::Muted)),
            Rect::new(
                area.x.saturating_add(1),
                area.bottom().saturating_sub(3),
                area.width.saturating_sub(3),
                1,
            ),
        );
        let detail = localmind
            .review
            .get(takeover.selected)
            .and_then(|row| {
                row.evidence
                    .as_deref()
                    .or(row.replacement.as_deref())
                    .or(row.note.as_deref())
            })
            .map_or_else(String::new, |text| format!("Evidence: {text}"));
        frame.render_widget(
            Paragraph::new(truncate_end(&detail, area.width.saturating_sub(3)))
                .style(theme.ui(UiRole::Muted)),
            Rect::new(
                area.x.saturating_add(1),
                area.bottom().saturating_sub(2),
                area.width.saturating_sub(3),
                1,
            ),
        );
    }
    let footer = match takeover.kind {
        TakeoverKind::Help => "↑/↓ scroll · Page Up/Page Down · Esc close".to_string(),
        TakeoverKind::Report => "↑/↓ scroll · Ctrl+C copy all · Esc close".to_string(),
        TakeoverKind::Sessions if area.width < 50 => "Enter resume · Esc return".to_string(),
        TakeoverKind::Sessions => "↑/↓ select · Enter resume · Esc return".to_string(),
        TakeoverKind::Settings => settings_footer(takeover, &setting_indices, area.width),
        TakeoverKind::Diff => {
            "↑/↓ navigate · ←/→ switch pane · t hide/show files · Esc close".to_string()
        }
        TakeoverKind::LocalMind => takeover.localmind.map_or_else(String::new, |localmind| {
            if localmind.editing_reviewer {
                "Type reviewer identity · Enter save · Esc cancel".to_string()
            } else if localmind.section == LocalMindSection::Review {
                "Tab/Shift+Tab section · ↑/↓ select · a accept · r reject · p promote · Esc close"
                    .to_string()
            } else {
                "Tab/Shift+Tab section · ↑/↓ scroll · Ctrl+C copy · Esc close".to_string()
            }
        }),
    };
    frame.render_widget(
        Paragraph::new(footer).style(theme.ui(UiRole::Muted)),
        Rect::new(
            area.x.saturating_add(1),
            area.bottom().saturating_sub(1),
            area.width.saturating_sub(3),
            1,
        ),
    );
    let dialog_hits = render_dialog(frame, area, content, app);

    HitMap {
        takeover: true,
        takeover_content: content,
        frame: None,
        tabs: Vec::new(),
        timelines: None,
        completion_rows: Vec::new(),
        theme_rows: Vec::new(),
        question_rows: dialog_hits.question_rows,
        trust_rows: dialog_hits.trust_rows,
        trust_path: dialog_hits.trust_path,
        takeover_rows,
        takeover_file_rows,
        takeover_scrollbar: scrollbar,
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
                crate::DiffLineKind::Addition => UiRole::DiffAddition,
                crate::DiffLineKind::Deletion => UiRole::DiffDeletion,
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
    let screen_reader = app.capabilities.screen_reader;
    let compact = frame_area.width < 50 || frame_area.height < 13;
    let width = if compact {
        frame_area.width
    } else {
        62.min(frame_area.width.saturating_sub(4))
    };
    let height = if compact {
        frame_area.height
    } else if screen_reader {
        12.min(frame_area.height)
    } else {
        13.min(frame_area.height.saturating_sub(2))
    };
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
    let inner = if screen_reader {
        frame.render_widget(Block::default().style(theme.ui(UiRole::Background)), area);
        frame.render_widget(
            Paragraph::new("Select a color mode").style(theme.ui(UiRole::Prompt)),
            Rect::new(area.x, area.y, area.width, 1),
        );
        area
    } else {
        let block = Block::bordered()
            .title(" Select a color mode ")
            .border_type(ratatui::widgets::BorderType::Rounded)
            .style(theme.ui(UiRole::Surface))
            .border_style(theme.ui(UiRole::Border));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        inner
    };
    if inner.height == 0 {
        return Vec::new();
    }
    let show_intro = !compact;
    if show_intro {
        frame.render_widget(
            Paragraph::new("Choose LocalPilot's semantic terminal colors.")
                .style(theme.ui(UiRole::Foreground)),
            Rect::new(
                inner.x,
                inner.y.saturating_add(u16::from(screen_reader)),
                inner.width,
                1,
            ),
        );
    }

    let option_start = inner
        .y
        .saturating_add(if screen_reader { 1 } else { 0 })
        .saturating_add(u16::from(show_intro));
    let option_width = if compact {
        inner.width
    } else {
        22.min(inner.width)
    };
    let mut hits = Vec::new();
    for (index, option) in Theme::ALL.iter().enumerate() {
        let row = u16::try_from(index).unwrap_or(u16::MAX);
        let y = option_start.saturating_add(row);
        if y >= inner.bottom() {
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
    if !compact && !screen_reader && preview_width > 0 {
        let sample = if app.capabilities.color == ColorSupport::NoColor {
            [
                ("- removed (underlined)", UiRole::DiffDeletion),
                ("+ added (bold)", UiRole::DiffAddition),
                ("  colors disabled", UiRole::Foreground),
                ("  selected", UiRole::Focus),
            ]
        } else {
            [
                ("1 - let total = items.len();", UiRole::DiffDeletion),
                ("1 + let item_count = items.len();", UiRole::DiffAddition),
                ("2   show(item_count);", UiRole::Code),
                ("3   // selected line", UiRole::Focus),
            ]
        };
        for (offset, (text, role)) in sample.into_iter().enumerate() {
            let row = u16::try_from(offset).unwrap_or(u16::MAX);
            let y = option_start.saturating_add(row);
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
    let option_end =
        option_start.saturating_add(u16::try_from(Theme::ALL.len()).unwrap_or(u16::MAX));
    let footer_y = inner.bottom().saturating_sub(1);
    let selection_y = footer_y.saturating_sub(1);
    let description_y = if screen_reader {
        selection_y.saturating_sub(1)
    } else {
        footer_y.saturating_sub(1)
    }
    .max(option_end.min(inner.bottom().saturating_sub(1)));
    frame.render_widget(
        Paragraph::new(theme_description(selected)).style(theme.ui(UiRole::Muted)),
        Rect::new(inner.x, description_y, inner.width, 1),
    );
    if screen_reader {
        frame.render_widget(
            Paragraph::new(format!(
                "Current selection: {}. {}",
                picker.selected + 1,
                selected.display_name()
            ))
            .style(theme.ui(UiRole::Focus)),
            Rect::new(inner.x, selection_y, inner.width, 1),
        );
    }
    let footer = if compact {
        "Enter select · Esc cancel"
    } else {
        "↑/↓ preview · Enter select · Esc cancel"
    };
    frame.render_widget(
        Paragraph::new(footer).style(theme.ui(UiRole::Muted)),
        Rect::new(inner.x, footer_y, inner.width, 1),
    );
    hits
}

fn theme_description(theme: Theme) -> &'static str {
    match theme {
        Theme::Terminal => "Use the terminal's base-16 palette",
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
    is_pair: bool,
) -> Vec<Line<'static>> {
    // Image attach is a single-session affordance; a pair session rejects it.
    let (image_line_1, image_line_2) = if is_pair {
        (
            "  Images      Image input is unavailable in pair sessions".to_string(),
            String::new(),
        )
    } else {
        (
            "  Ctrl+V      Attach an image (vision-capable models): a copied".to_string(),
            "              bitmap or image file, or a pasted/dropped image path".to_string(),
        )
    };
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
            "  Ctrl+S      Stash a draft, or restore the saved draft".to_string(),
            UiRole::Foreground,
        ),
        (image_line_1, UiRole::Foreground),
        (image_line_2, UiRole::Foreground),
        (
            "  Esc Esc     Clear the current draft".to_string(),
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
                "  Drag / icon Select text; click a tool status to expand it".to_string()
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
            "  Ctrl+C      Copy selection; else clear draft, cancel work, then exit".to_string(),
            UiRole::Foreground,
        ),
        (String::new(), UiRole::Foreground),
        (
            "LocalPilot keeps provider, permission, and tool behavior unchanged in this view."
                .to_string(),
            UiRole::Muted,
        ),
    ]);

    text_takeover_lines(&source, width, theme)
}

/// Wrap and style a takeover's `(text, role)` source into rendered lines. Shared
/// by the Help takeover and the command Report takeover so their line-list
/// wrapping and styling cannot drift apart.
fn text_takeover_lines(
    source: &[(String, UiRole)],
    width: u16,
    theme: ThemeResolver,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (text, role) in source {
        for range in crate::text::wrap_ranges(text, width) {
            lines.push(Line::styled(
                text[range.start_byte..range.end_byte].to_string(),
                theme.ui(*role),
            ));
        }
    }
    lines
}

/// Wrap a bounded plain-text takeover body while allocating only the visible
/// window. The first pass computes an exact wrapped-row count so a resize can
/// clamp stale scroll positions; the second materializes at most one viewport.
fn text_takeover_window(
    source: &[String],
    requested_start: usize,
    viewport_rows: usize,
    width: u16,
    theme: ThemeResolver,
) -> (usize, usize, Vec<Line<'static>>) {
    let total = source
        .iter()
        .map(|text| crate::text::wrap_ranges(text, width).len())
        .sum::<usize>();
    let start = requested_start.min(total.saturating_sub(viewport_rows));
    let end = start.saturating_add(viewport_rows);
    let mut index = 0usize;
    let mut visible = Vec::with_capacity(viewport_rows.min(total));
    for text in source {
        for range in crate::text::wrap_ranges(text, width) {
            if index >= start && index < end {
                visible.push(Line::styled(
                    text[range.start_byte..range.end_byte].to_string(),
                    theme.ui(UiRole::Foreground),
                ));
            }
            index = index.saturating_add(1);
            if index >= end && visible.len() == viewport_rows {
                return (start, total, visible);
            }
        }
    }
    (start, total, visible)
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
    if app.capabilities.screen_reader {
        let remaining = app
            .tabs
            .iter()
            .filter(|tab| **tab != app.active_tab)
            .map(|tab| tab.label())
            .collect::<Vec<_>>()
            .join(", ");
        let sentence = if remaining.is_empty() {
            format!("Home: current tab: {}", app.active_tab.label())
        } else {
            format!(
                "Home: current tab: {}; tabs: {remaining}",
                app.active_tab.label()
            )
        };
        frame.render_widget(
            Paragraph::new(sentence)
                .style(theme.ui(UiRole::Foreground))
                .wrap(Wrap { trim: false }),
            area,
        );
        return Vec::new();
    }
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

fn render_peer_label(
    frame: &mut Frame<'_>,
    timeline: TimelinePaneLayout,
    peer: Option<PeerPane>,
    projection: &SessionProjection,
    app: &AppModel,
) {
    let (Some(label), Some(peer)) = (timeline.label, peer) else {
        return;
    };
    let peer_name = match peer {
        PeerPane::A => "A",
        PeerPane::B => "B",
    };
    let provider = sanitize_inline(&projection.header.provider);
    let model = sanitize_inline(&projection.header.model);
    let active = app.active_pair_pane() == Some(peer);
    let marker = if active { " [active]" } else { "" };
    Paragraph::new(format!(" Peer {peer_name}{marker} · {provider} · {model}"))
        .style(theme(app).ui(if active { UiRole::Focus } else { UiRole::Muted }))
        .render(label, frame.buffer_mut());
}

fn render_peer_divider(frame: &mut Frame<'_>, area: Rect, app: &AppModel) {
    for y in area.y..area.bottom() {
        Line::styled("│", theme(app).ui(UiRole::Border)).render(
            Rect::new(area.x, y, area.width.min(1), 1),
            frame.buffer_mut(),
        );
    }
}

fn render_timeline(
    frame: &mut Frame<'_>,
    timeline: TimelinePaneLayout,
    peer: Option<PeerPane>,
    projection: &SessionProjection,
    app: &AppModel,
) -> TimelinePaneHits {
    let area = timeline.content;
    let wrap_width = timeline_wrap_width(area.width, app);
    let view = projection.timeline.view(wrap_width, area.height.max(1));
    let banner_visible = view.pinned.is_none()
        && view.start == 0
        && (view.total_rows.saturating_add(usize::from(BANNER_ROWS)) <= usize::from(area.height)
            || matches!(projection.timeline.viewport, crate::ViewportAnchor::Top));
    let content_offset = if banner_visible {
        render_idle_banner(frame, area, projection, app);
        BANNER_ROWS
    } else if let Some(pinned) = &view.pinned {
        render_pinned_prompt(frame, area, pinned, app);
        u16::try_from(PinnedPrompt::ROWS).unwrap_or(u16::MAX)
    } else {
        0
    };
    let mut row_hits = Vec::new();
    if view.rows.is_empty() && view.pinned.is_none() && !banner_visible {
        render_idle_banner(frame, area, projection, app);
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
                let (first, last) = match row.part {
                    VisualRowPart::Content { first, last } => (first, last),
                    VisualRowPart::FrameTop | VisualRowPart::FrameBottom => (false, false),
                };
                let content_column = role_prefix(
                    row.kind,
                    row.activity,
                    row.tone,
                    first,
                    last,
                    theme(app),
                    app.capabilities.screen_reader,
                )
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum::<usize>();
                row_hits.push(TimelineRowHit {
                    y,
                    content_x: area
                        .x
                        .saturating_add(u16::try_from(content_column).unwrap_or(u16::MAX)),
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
        if app.capabilities.screen_reader {
            Rect::default()
        } else {
            timeline.scrollbar
        },
        view.start,
        view.total_rows,
        scrollbar_viewport_rows,
    );
    draw_scrollbar(frame, scrollbar, app);
    TimelinePaneHits {
        peer,
        label: timeline.label,
        viewport: timeline.viewport,
        timeline: area,
        wrap_width,
        rows: row_hits,
        scrollbar,
    }
}

fn timeline_wrap_width(width: u16, app: &AppModel) -> u16 {
    if app.capabilities.screen_reader {
        width.saturating_sub(SCREEN_READER_PREFIX_EXTRA).max(1)
    } else {
        width.max(1)
    }
}

fn render_idle_banner(
    frame: &mut Frame<'_>,
    area: Rect,
    projection: &SessionProjection,
    app: &AppModel,
) {
    let theme = theme(app);
    if app.capabilities.screen_reader {
        let rows = [
            format!(
                "{APP_NAME} v{} uses AI.",
                app.shared_version()
                    .strip_prefix('v')
                    .unwrap_or(app.shared_version())
            ),
            "Check important results.".to_string(),
            String::new(),
            format!(
                "{} · {}",
                projection.header.provider, projection.header.model
            ),
            "Tip: press ? for shortcuts".to_string(),
            "Type / to browse commands".to_string(),
        ];
        for (offset, row) in rows.into_iter().take(usize::from(area.height)).enumerate() {
            Line::styled(row, theme.ui(UiRole::Foreground)).render(
                Rect::new(area.x, area.y.saturating_add(offset as u16), area.width, 1),
                frame.buffer_mut(),
            );
        }
        return;
    }
    let mark = theme.ui(UiRole::Accent);
    let rows = vec![
        Line::from(vec![
            Span::styled("╭──────╮", mark),
            Span::styled(
                format!(
                    "  {APP_NAME} v{}",
                    app.shared_version()
                        .strip_prefix('v')
                        .unwrap_or(app.shared_version())
                ),
                theme.ui(UiRole::Foreground),
            ),
        ]),
        Line::from(vec![
            Span::styled("│ >_ ● │", mark),
            Span::styled(
                format!(
                    "  {} · {}",
                    projection.header.provider, projection.header.model
                ),
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
    if app.capabilities.screen_reader {
        Line::styled("User message", theme.ui(UiRole::Prompt))
            .render(Rect::new(area.x, area.y, area.width, 1), frame.buffer_mut());
        let pending = if pin.pending { " (pending)" } else { "" };
        let text = format!("  {}{pending}", pin.text);
        let trailing = pin.trailing.as_deref().unwrap_or("");
        Line::styled(
            two_sided(&text, trailing, area.width),
            theme.ui(UiRole::Foreground),
        )
        .render(
            Rect::new(area.x, area.y.saturating_add(1), area.width, 1),
            frame.buffer_mut(),
        );
        return;
    }
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
        if app.capabilities.screen_reader {
            return Line::styled("User message", theme.ui(UiRole::Prompt));
        }
        return framed_rule(width, true, theme.ui(UiRole::SurfaceEdge));
    }
    if row.part == VisualRowPart::FrameBottom {
        if app.capabilities.screen_reader {
            return Line::default();
        }
        return framed_rule(width, false, theme.ui(UiRole::SurfaceEdge));
    }

    let VisualRowPart::Content { first, last } = row.part else {
        return Line::default();
    };
    let mut spans = role_prefix(
        row.kind,
        row.activity,
        row.tone,
        first,
        last,
        theme,
        app.capabilities.screen_reader,
    );
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
                app.active_timeline()
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
    if row.kind == ItemKind::User && !app.capabilities.screen_reader {
        line.style(theme.ui(UiRole::Surface))
    } else {
        line
    }
}

fn framed_rule(width: u16, top: bool, style: ratatui::style::Style) -> Line<'static> {
    let fill = if top { "▄" } else { "▀" };
    Line::styled(fill.repeat(usize::from(width)), style)
}

/// The theme role a retained result renders with. Absent tone fails closed to
/// `Error` so a result never wears success chrome without a proved convergence;
/// only an explicit `Success` tone (set exclusively by `push_result`) is success.
const fn result_role(tone: Option<crate::ResultTone>) -> UiRole {
    match tone {
        Some(crate::ResultTone::Success) => UiRole::Success,
        Some(crate::ResultTone::Incomplete) => UiRole::Warning,
        Some(crate::ResultTone::Error) | None => UiRole::Error,
    }
}

fn role_prefix(
    kind: ItemKind,
    activity: Option<ActivityState>,
    tone: Option<crate::ResultTone>,
    first: bool,
    last: bool,
    theme: ThemeResolver,
    screen_reader: bool,
) -> Vec<Span<'static>> {
    if screen_reader {
        let (label, role) = match kind {
            ItemKind::User | ItemKind::Assistant => ("  ", UiRole::Foreground),
            ItemKind::Reasoning => (
                if first { "Reasoning: " } else { "           " },
                UiRole::Muted,
            ),
            ItemKind::Tool => match activity {
                Some(ActivityState::Success) => (
                    if first {
                        "Tool completed: "
                    } else {
                        "                "
                    },
                    UiRole::Success,
                ),
                Some(ActivityState::Error) => (
                    if first {
                        "Tool failed: "
                    } else {
                        "             "
                    },
                    UiRole::Error,
                ),
                Some(ActivityState::Cancelled) => (
                    if first {
                        "Tool cancelled: "
                    } else {
                        "                "
                    },
                    UiRole::Muted,
                ),
                Some(ActivityState::Running) | None => (
                    if first {
                        "Tool running: "
                    } else {
                        "              "
                    },
                    UiRole::Code,
                ),
            },
            ItemKind::Question => ("  ", UiRole::Foreground),
            ItemKind::Shell => match activity {
                Some(ActivityState::Success) => (
                    if first {
                        "Shell completed: "
                    } else {
                        "                 "
                    },
                    UiRole::Success,
                ),
                Some(ActivityState::Error) => (
                    if first {
                        "Shell failed: "
                    } else {
                        "              "
                    },
                    UiRole::Error,
                ),
                Some(ActivityState::Cancelled) => (
                    if first {
                        "Shell cancelled: "
                    } else {
                        "                 "
                    },
                    UiRole::Muted,
                ),
                Some(ActivityState::Running) | None => (
                    if first {
                        "Shell running: "
                    } else {
                        "               "
                    },
                    UiRole::Code,
                ),
            },
            ItemKind::Notice => (if first { "Notice: " } else { "        " }, UiRole::Warning),
            ItemKind::Result => (
                if first { "Result: " } else { "        " },
                result_role(tone),
            ),
        };
        return vec![Span::styled(label, theme.ui(role))];
    }
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
                Some(ActivityState::Cancelled) => ("■ ", UiRole::Muted),
            };
            vec![Span::styled(
                if first {
                    glyph
                } else if last {
                    "└ "
                } else {
                    "│ "
                },
                theme.ui(if first { role } else { UiRole::Muted }),
            )]
        }
        ItemKind::Question => {
            let (glyph, role) = match activity {
                Some(ActivityState::Running) | None => ("○ ", UiRole::Foreground),
                Some(ActivityState::Success) => ("● ", UiRole::Foreground),
                Some(ActivityState::Error | ActivityState::Cancelled) => ("● ", UiRole::Muted),
            };
            vec![Span::styled(
                if first { glyph } else { "└ " },
                theme.ui(role),
            )]
        }
        ItemKind::Shell => {
            let (glyph, role) = match activity {
                Some(ActivityState::Error) => ("✗ ", UiRole::Error),
                Some(ActivityState::Success) => ("$ ", UiRole::Success),
                Some(ActivityState::Cancelled) => ("■ ", UiRole::Muted),
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
        ItemKind::Result => vec![Span::styled(
            if first { "◆ " } else { "  " },
            theme.ui(result_role(tone)),
        )],
    }
}

fn render_status(frame: &mut Frame<'_>, area: Rect, app: &AppModel, narrow: bool) {
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
    app.shared_branch().map_or_else(
        || app.shared_workspace().to_string(),
        |branch| {
            format!(
                "{} [{}{}]",
                app.shared_workspace(),
                branch,
                if app.shared_workspace_dirty() == Some(true) {
                    "*"
                } else {
                    ""
                }
            )
        },
    )
}

fn status_right(app: &AppModel) -> String {
    let usage = app.active_usage().unwrap_or_default();
    let mut parts = Vec::new();
    if let Some(status) = app.pair_status() {
        if let Some(terminal) = &status.terminal {
            parts.push(format!(
                "{terminal} · {}/{} rounds",
                status.completed_rounds, status.max_rounds
            ));
        } else {
            let current = if status.max_rounds == 0 {
                0
            } else {
                status
                    .completed_rounds
                    .saturating_add(1)
                    .min(status.max_rounds)
            };
            let mut running = format!("{current}/{} rounds", status.max_rounds);
            if let Some(peer) = status.scheduled {
                running.push_str(&format!(" · Peer {}", peer_label(peer)));
            }
            if let Some(candidate) = &status.candidate {
                running.push_str(&format!(
                    " · r{} {}",
                    candidate.revision,
                    abbreviated_digest(&candidate.full_digest)
                ));
            }
            running.push_str(&format!(
                " · A {} · B {}",
                agreement_word(status.agreements[0]),
                agreement_word(status.agreements[1])
            ));
            if let Some(peer) = status.repairing {
                running.push_str(&format!(" · Repairing Peer {}", peer_label(peer)));
            }
            parts.push(running);
        }
    }
    parts.push(format!("{} tokens", usage.total()));
    if usage.cached_input_tokens > 0 {
        parts.push(format!("{} cached", usage.cached_input_tokens));
    }
    if let Some((used, limit)) = app.active_context_usage() {
        let percentage = if limit == 0 {
            0
        } else {
            used.saturating_mul(100) / limit
        };
        parts.push(format!("{percentage}% context"));
    }
    parts.join(" · ")
}

const fn peer_label(peer: crate::PeerPane) -> &'static str {
    match peer {
        crate::PeerPane::A => "A",
        crate::PeerPane::B => "B",
    }
}

const fn agreement_word(agreed: bool) -> &'static str {
    if agreed {
        "agreed"
    } else {
        "pending"
    }
}

/// The first eight characters of a full candidate digest, for compact chrome.
fn abbreviated_digest(full_digest: &str) -> String {
    full_digest.chars().take(8).collect()
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
        render_pair_composer_label(frame, layout.composer, app, surface);
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
    render_pair_composer_label(frame, layout.composer, app, surface);
    let empty = app.editor.text().is_empty();
    let placeholder_text: Option<&str> = if empty && app.shell_mode() {
        Some("Run a shell command")
    } else if empty {
        // Host-projected mode hint (e.g. Research); the renderer never parses a mode
        // string. Shown only when the editor is empty, so the cursor stays at origin
        // and composer geometry is unchanged.
        app.composer_hint()
    } else {
        None
    };
    let placeholder = placeholder_text.is_some();
    let composer_text = placeholder_text.unwrap_or_else(|| app.editor.text());
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

fn render_pair_composer_label(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &AppModel,
    surface: ratatui::style::Style,
) {
    let Some(peer) = app.active_pair_pane() else {
        return;
    };
    let width = area.width.saturating_sub(4);
    if width == 0 {
        return;
    }
    let peer = match peer {
        crate::PeerPane::A => "A",
        crate::PeerPane::B => "B",
    };
    let label = truncate_end(&format!(" Steer Peer {peer} "), width);
    frame.render_widget(
        Paragraph::new(label).style(surface.patch(theme(app).ui(UiRole::Prompt))),
        Rect::new(
            area.x.saturating_add(2),
            area.y,
            width.min(area.right().saturating_sub(area.x.saturating_add(2))),
            1,
        ),
    );
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

fn render_dialog(
    frame: &mut Frame<'_>,
    frame_area: Rect,
    timeline_area: Rect,
    app: &AppModel,
) -> DialogHits {
    let Some(dialog) = &app.dialog else {
        return DialogHits::default();
    };
    if let DialogState::Trust(_) = dialog {
        return app.trust().map_or_else(DialogHits::default, |trust| {
            let minimum_height = if app.capabilities.screen_reader {
                8
            } else {
                10
            };
            let trust_area = if timeline_area.height >= minimum_height {
                timeline_area
            } else {
                frame_area
            };
            let hits = render_trust_dialog(
                frame,
                trust_area,
                app,
                trust,
                app.capabilities.screen_reader,
            );
            DialogHits {
                trust_rows: hits.rows,
                trust_path: hits.path,
                ..DialogHits::default()
            }
        });
    }
    if app.capabilities.screen_reader {
        return DialogHits {
            question_rows: render_screen_reader_dialog(frame, frame_area, app, dialog),
            ..DialogHits::default()
        };
    }
    if let DialogState::Question(_) = dialog {
        return app
            .question()
            .map_or_else(DialogHits::default, |question| DialogHits {
                question_rows: render_question_dialog(frame, frame_area, app, question, false),
                ..DialogHits::default()
            });
    }
    let width = frame_area.width.saturating_sub(4).min(72);
    let height = frame_area.height.saturating_sub(2).min(7);
    if width < 20 || height < 5 {
        return DialogHits::default();
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
        DialogState::Trust(_) => Vec::new(),
        DialogState::Approval {
            tool,
            target,
            risk_class,
        } => vec![
            Line::from(vec![
                Span::styled("● ", theme.ui(UiRole::Warning)),
                Span::styled(
                    dialog_heading(app, "Permission required"),
                    theme.ui(UiRole::Prompt),
                ),
            ]),
            Line::styled(
                format!("{tool} · {risk_class}"),
                theme.ui(UiRole::Foreground),
            ),
            Line::styled(target.clone(), theme.ui(UiRole::Muted)),
            Line::styled(
                if app.dialog_peer().is_some() {
                    "Ctrl+C abort · Y allow once · N or Esc deny"
                } else {
                    "Y allow once · N deny"
                },
                theme.ui(UiRole::Muted),
            ),
        ],
        DialogState::Question(_) => Vec::new(),
    };
    frame.render_widget(
        Paragraph::new(lines)
            .style(theme.ui(UiRole::Surface))
            .wrap(Wrap { trim: false }),
        inner,
    );
    DialogHits::default()
}

fn dialog_heading(app: &AppModel, heading: &str) -> String {
    app.dialog_peer().map_or_else(
        || heading.to_string(),
        |peer| {
            let peer = match peer {
                PeerPane::A => "A",
                PeerPane::B => "B",
            };
            // Read the origin peer's model from its named projection and sanitize it
            // here; a malformed or empty label never drops the promised model text.
            let model = app
                .dialog_peer_model()
                .map(sanitize_inline)
                .unwrap_or_default();
            let model = if model.trim().is_empty() {
                "unknown model"
            } else {
                model.as_str()
            };
            format!("Peer {peer} · {model} · {heading}")
        },
    )
}

fn render_trust_dialog(
    frame: &mut Frame<'_>,
    timeline_area: Rect,
    app: &AppModel,
    trust: TrustView<'_>,
    screen_reader: bool,
) -> TrustRenderHits {
    let requested_height = if screen_reader { 9 } else { 11 };
    let height = timeline_area.height.min(requested_height);
    let minimum_height = if screen_reader { 8 } else { 10 };
    if timeline_area.width < 20 || height < minimum_height {
        return TrustRenderHits::default();
    }
    let area = Rect::new(
        timeline_area.x,
        timeline_area.y + timeline_area.height.saturating_sub(height) / 2,
        timeline_area.width,
        height,
    );
    let theme = theme(app);
    frame.render_widget(Clear, timeline_area);
    frame.render_widget(
        Block::default().style(theme.ui(UiRole::Background)),
        timeline_area,
    );

    if screen_reader {
        let content_x = area.x.saturating_add(1);
        let content_width = area.width.saturating_sub(2);
        let shown_path = middle_elide(trust.path, content_width);
        let mut rows = vec![
            Line::styled("Trust this workspace?", theme.ui(UiRole::Prompt)),
            trust_path_line(&shown_path, trust, app),
            Line::styled(
                "Choose how LocalPilot may use this workspace.",
                theme.ui(UiRole::Muted),
            ),
        ];
        rows.extend(TRUST_OPTIONS.iter().enumerate().map(|(index, label)| {
            Line::styled(
                truncate_end(&format!("{}. {label}", index + 1), content_width),
                theme.ui(if trust.selected == index {
                    UiRole::Focus
                } else {
                    UiRole::Foreground
                }),
            )
        }));
        rows.push(Line::styled(
            truncate_end(
                &format!(
                    "Current selection: {}. {}",
                    trust.selected + 1,
                    TRUST_OPTIONS[trust.selected.min(TRUST_OPTIONS.len() - 1)]
                ),
                content_width,
            ),
            theme.ui(UiRole::Foreground),
        ));
        rows.push(Line::styled(
            "Up/Down select · Enter confirm · Escape exit",
            theme.ui(UiRole::Muted),
        ));
        frame.render_widget(
            Paragraph::new(rows)
                .style(theme.ui(UiRole::Background))
                .wrap(Wrap { trim: false }),
            Rect::new(content_x, area.y, content_width, area.height),
        );
        let rows = TRUST_OPTIONS
            .iter()
            .enumerate()
            .map(|(index, _)| TrustHit {
                index,
                area: Rect::new(
                    content_x,
                    area.y
                        .saturating_add(3 + u16::try_from(index).unwrap_or(u16::MAX)),
                    content_width,
                    1,
                ),
            })
            .collect();
        return TrustRenderHits {
            rows,
            path: Some(TrustPathHit {
                area: Rect::new(content_x, area.y.saturating_add(1), content_width, 1),
                text: shown_path,
            }),
        };
    }

    let block = Block::bordered()
        .border_type(ratatui::widgets::BorderType::Rounded)
        .style(theme.ui(UiRole::Surface))
        .border_style(theme.ui(UiRole::Border));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let content_x = inner.x.saturating_add(2);
    let content_width = inner.width.saturating_sub(4);
    frame.render_widget(
        Paragraph::new("Trust this workspace?").style(theme.ui(UiRole::Prompt)),
        Rect::new(content_x, inner.y, content_width, 1),
    );
    let compact = area.height < requested_height;
    if !compact {
        frame.render_widget(
            Paragraph::new("Choose how LocalPilot may use this workspace.")
                .style(theme.ui(UiRole::Muted)),
            Rect::new(content_x, inner.y.saturating_add(1), content_width, 1),
        );
    }
    let path_area = Rect::new(
        content_x,
        inner.y.saturating_add(if compact { 1 } else { 2 }),
        content_width,
        3,
    );
    let path_block = Block::bordered()
        .border_type(ratatui::widgets::BorderType::Rounded)
        .style(theme.ui(UiRole::Surface))
        .border_style(theme.ui(UiRole::Border));
    let path_inner = path_block.inner(path_area);
    frame.render_widget(path_block, path_area);
    let shown_path = middle_elide(trust.path, path_inner.width);
    frame.render_widget(
        Paragraph::new(trust_path_line(&shown_path, trust, app)),
        path_inner,
    );

    let choices_y = path_area.bottom();
    let footer_y = inner.bottom().saturating_sub(1);
    let mut hits = Vec::new();
    for (index, label) in TRUST_OPTIONS.iter().enumerate() {
        let y = choices_y.saturating_add(u16::try_from(index).unwrap_or(u16::MAX));
        if y >= footer_y {
            break;
        }
        let selected = trust.selected == index;
        let shown = format!(
            "{} {}. {label}",
            if selected { "❯" } else { " " },
            index + 1
        );
        let hit = Rect::new(content_x, y, content_width, 1);
        frame.render_widget(
            Paragraph::new(truncate_end(&shown, content_width)).style(theme.ui(if selected {
                UiRole::Focus
            } else {
                UiRole::Foreground
            })),
            hit,
        );
        hits.push(TrustHit { index, area: hit });
    }
    frame.render_widget(
        Paragraph::new(truncate_end(
            "↑/↓ to select · enter to confirm · esc to exit",
            content_width,
        ))
        .style(theme.ui(UiRole::Muted)),
        Rect::new(content_x, footer_y, content_width, 1),
    );
    TrustRenderHits {
        rows: hits,
        path: Some(TrustPathHit {
            area: path_inner,
            text: shown_path,
        }),
    }
}

fn trust_path_line<'a>(shown_path: &'a str, trust: TrustView<'_>, app: &AppModel) -> Line<'a> {
    let theme = theme(app);
    let Some(selection) = trust
        .path_selection
        .filter(|selection| selection.source == shown_path && selection.start < selection.end)
    else {
        return Line::styled(shown_path, theme.ui(UiRole::Foreground));
    };
    let start = previous_grapheme_boundary(shown_path, selection.start.min(shown_path.len()));
    let end = previous_grapheme_boundary(shown_path, selection.end.min(shown_path.len()));
    Line::from(vec![
        Span::styled(&shown_path[..start], theme.ui(UiRole::Foreground)),
        Span::styled(&shown_path[start..end], theme.ui(UiRole::Selection)),
        Span::styled(&shown_path[end..], theme.ui(UiRole::Foreground)),
    ])
}

fn previous_grapheme_boundary(text: &str, byte: usize) -> usize {
    if byte >= text.len() {
        return text.len();
    }
    text.grapheme_indices(true)
        .map(|(start, _)| start)
        .take_while(|start| *start <= byte)
        .last()
        .unwrap_or(0)
}

fn render_screen_reader_dialog(
    frame: &mut Frame<'_>,
    frame_area: Rect,
    app: &AppModel,
    dialog: &DialogState,
) -> Vec<QuestionHit> {
    if let DialogState::Question(_) = dialog {
        return app.question().map_or_else(Vec::new, |question| {
            render_question_dialog(frame, frame_area, app, question, true)
        });
    }
    let width = frame_area.width.saturating_sub(4).min(72);
    let height = frame_area.height.saturating_sub(2).min(7);
    if width < 20 || height < 5 {
        return Vec::new();
    }
    let area = Rect::new(
        frame_area.x + frame_area.width.saturating_sub(width) / 2,
        frame_area.y + frame_area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let theme = theme(app);
    let lines = match dialog {
        DialogState::Trust(_) => Vec::new(),
        DialogState::Approval {
            tool,
            target,
            risk_class,
        } => vec![
            Line::styled(
                dialog_heading(app, "Permission required"),
                theme.ui(UiRole::Prompt),
            ),
            Line::styled(
                format!("{tool} · {risk_class}"),
                theme.ui(UiRole::Foreground),
            ),
            Line::styled(truncate_end(target, area.width), theme.ui(UiRole::Muted)),
            Line::styled("Y allow once", theme.ui(UiRole::Foreground)),
            Line::styled(
                if app.dialog_peer().is_some() {
                    "N or Esc deny · Ctrl+C abort"
                } else {
                    "N or Esc deny"
                },
                theme.ui(UiRole::Muted),
            ),
        ],
        DialogState::Question(_) => Vec::new(),
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .style(theme.ui(UiRole::Background))
            .wrap(Wrap { trim: false }),
        area,
    );
    Vec::new()
}

fn render_question_dialog(
    frame: &mut Frame<'_>,
    frame_area: Rect,
    app: &AppModel,
    question: QuestionView<'_>,
    screen_reader: bool,
) -> Vec<QuestionHit> {
    let width = frame_area
        .width
        .saturating_sub(4)
        .min(if screen_reader { 72 } else { 44 });
    let horizontal_chrome = if screen_reader { 0 } else { 4 };
    let projected_content_width = width.saturating_sub(horizontal_chrome);
    // A collaboration question can be aborted (Ctrl+C) as well as dismissed (Esc),
    // including while editing the Other field. Lead with BOTH controls so each survives
    // width truncation. Single chat keeps its copy byte-identical.
    let footer = if app.dialog_peer().is_some() {
        if question.editing_other {
            "Ctrl+C abort · Esc choices · Enter · H/E".to_string()
        } else if question.multi_select {
            "Ctrl+C abort · Esc dismiss · ↑/↓ select · space toggle · enter confirm".to_string()
        } else {
            "Ctrl+C abort · Esc dismiss · ↑/↓ select · enter confirm".to_string()
        }
    } else if question.editing_other {
        "Enter · Esc choices · Home/End".to_string()
    } else if question.multi_select {
        "↑/↓ to select · space to toggle · enter to confirm · esc to cancel".to_string()
    } else {
        "↑/↓ to select · enter to confirm · esc to cancel".to_string()
    };
    let footer_rows = if screen_reader
        && UnicodeWidthStr::width(footer.as_str()) > usize::from(projected_content_width)
    {
        2
    } else {
        1
    };
    let other_prefix = format!(
        "{} {}. ",
        if question.selected == question.options.len() {
            "❯"
        } else {
            " "
        },
        question.options.len() + 1
    );
    let other_prefix_width =
        u16::try_from(UnicodeWidthStr::width(other_prefix.as_str())).unwrap_or(u16::MAX);
    // Keep one cell for a proportional scrollbar. Reserving it even before the
    // answer overflows prevents the text from re-wrapping when the bar appears.
    let other_editor_width = projected_content_width
        .saturating_sub(other_prefix_width)
        .saturating_sub(1)
        .max(1);
    let other_rows = if question.editing_other {
        crate::text::wrap_ranges(question.other, other_editor_width)
    } else {
        Vec::new()
    };
    let extra_other_rows = u16::try_from(other_rows.len().saturating_sub(1)).unwrap_or(u16::MAX);
    let fixed_rows = if screen_reader { 3 } else { 6 };
    let requested_height = u16::try_from(question.options.len())
        .unwrap_or(u16::MAX)
        .saturating_add(fixed_rows)
        .saturating_add(footer_rows)
        .saturating_add(extra_other_rows);
    let height = frame_area.height.saturating_sub(2).min(requested_height);
    let minimum_height = if screen_reader { 6 } else { 9 };
    if width < 20 || height < minimum_height {
        return Vec::new();
    }
    let area = Rect::new(
        frame_area.x + frame_area.width.saturating_sub(width) / 2,
        frame_area.y + frame_area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let theme = theme(app);
    frame.render_widget(Clear, area);
    let inner = if screen_reader {
        frame.render_widget(Block::default().style(theme.ui(UiRole::Background)), area);
        area
    } else {
        let block = Block::bordered()
            .border_type(ratatui::widgets::BorderType::Rounded)
            .style(theme.ui(UiRole::Surface))
            .border_style(theme.ui(UiRole::Border));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        inner
    };
    if inner.height < 5 {
        return Vec::new();
    }

    let left = inner.x.saturating_add(u16::from(!screen_reader));
    let content_width = inner
        .right()
        .saturating_sub(left)
        .saturating_sub(u16::from(!screen_reader));
    let heading = match (question.header, question.total > 1) {
        (Some(header), true) => format!("{header}  ({}/{})", question.index, question.total),
        (Some(header), false) => header.to_string(),
        (None, true) => format!("Question {}/{}", question.index, question.total),
        (None, false) => "Question".to_string(),
    };
    let heading = dialog_heading(app, &heading);
    frame.render_widget(
        Paragraph::new(truncate_end(&heading, content_width)).style(theme.ui(UiRole::Prompt)),
        Rect::new(left, inner.y, content_width, 1),
    );
    if !screen_reader {
        frame.render_widget(
            Paragraph::new("─".repeat(usize::from(content_width))).style(theme.ui(UiRole::Border)),
            Rect::new(left, inner.y.saturating_add(1), content_width, 1),
        );
    }
    let question_y = inner.y.saturating_add(if screen_reader { 1 } else { 2 });
    frame.render_widget(
        Paragraph::new(truncate_end(question.question, content_width))
            .style(theme.ui(UiRole::Foreground)),
        Rect::new(left, question_y, content_width, 1),
    );

    let options_y = question_y.saturating_add(1);
    let footer_y = inner.bottom().saturating_sub(footer_rows);
    let mut hits = Vec::new();
    for index in 0..=question.options.len() {
        let y = options_y.saturating_add(u16::try_from(index).unwrap_or(u16::MAX));
        if y >= footer_y {
            break;
        }
        let selected = question.selected == index;
        let marker = if selected { "❯" } else { " " };
        if index == question.options.len() && question.editing_other {
            let viewport_rows = usize::from(footer_y.saturating_sub(y))
                .min(other_rows.len())
                .max(1);
            let viewport_height = u16::try_from(viewport_rows).unwrap_or(u16::MAX);
            let (cursor_row, cursor_column) = crate::editor::text_row_and_column(
                question.other,
                question.other_cursor,
                other_editor_width,
            );
            let scroll = cursor_row
                .saturating_add(1)
                .saturating_sub(viewport_rows)
                .min(other_rows.len().saturating_sub(viewport_rows));
            let prefix_width = other_prefix_width.min(content_width.saturating_sub(1));
            let answer_area = Rect::new(
                left.saturating_add(prefix_width),
                y,
                other_editor_width.min(content_width.saturating_sub(prefix_width)),
                viewport_height,
            );
            let answer_lines = if question.other.is_empty() {
                vec![Line::styled("Type your answer", theme.ui(UiRole::Muted))]
            } else {
                other_rows
                    .iter()
                    .map(|row| Line::raw(question.other[row.start_byte..row.end_byte].to_string()))
                    .collect::<Vec<_>>()
            };
            frame.render_widget(
                Paragraph::new(truncate_end(&other_prefix, prefix_width))
                    .style(theme.ui(UiRole::Focus)),
                Rect::new(left, y, prefix_width, 1),
            );
            frame.render_widget(
                Paragraph::new(answer_lines)
                    .style(theme.ui(UiRole::Focus))
                    .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
                answer_area,
            );
            let scrollbar = ScrollbarGeometry::calculate(
                Rect::new(answer_area.right(), y, 1, viewport_height),
                scroll,
                other_rows.len(),
                viewport_rows,
            );
            draw_scrollbar(frame, scrollbar, app);
            let hit = Rect::new(left, y, content_width, viewport_height);
            hits.push(QuestionHit { index, area: hit });
            frame.set_cursor_position((
                answer_area
                    .x
                    .saturating_add(cursor_column)
                    .min(answer_area.right().saturating_sub(1)),
                answer_area
                    .y
                    .saturating_add(
                        u16::try_from(cursor_row.saturating_sub(scroll)).unwrap_or(u16::MAX),
                    )
                    .min(answer_area.bottom().saturating_sub(1)),
            ));
            continue;
        }
        let label = if let Some(option) = question.options.get(index) {
            option.label.as_str()
        } else {
            "Other (type your answer)"
        };
        let selection_mark = if index < question.options.len() && question.multi_select {
            if question.checked.get(index).copied().unwrap_or(false) {
                "[x] "
            } else {
                "[ ] "
            }
        } else {
            ""
        };
        let description = question
            .options
            .get(index)
            .and_then(|option| option.description.as_deref())
            .map_or(String::new(), |description| format!(" — {description}"));
        let shown = format!(
            "{marker} {}. {selection_mark}{label}{description}",
            index + 1
        );
        let role = if selected {
            UiRole::Focus
        } else {
            UiRole::Foreground
        };
        let hit = Rect::new(left, y, content_width, 1);
        frame.render_widget(
            Paragraph::new(truncate_end(&shown, content_width)).style(theme.ui(role)),
            hit,
        );
        hits.push(QuestionHit { index, area: hit });
    }
    let footer = if screen_reader {
        footer.clone()
    } else {
        truncate_end(&footer, content_width)
    };
    frame.render_widget(
        Paragraph::new(footer)
            .style(theme.ui(UiRole::Muted))
            .wrap(Wrap { trim: false }),
        Rect::new(left, footer_y, content_width, footer_rows),
    );
    hits
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
    let activity = working_status(app);
    let state = footer_state_with_activity(app, activity.as_deref());
    let shortcuts = if app.is_pair() {
        "F6 peer · ? help · / commands"
    } else {
        "? help · / commands"
    };
    let context = if matches!(app.active_work(), crate::WorkState::Busy { .. }) {
        app.active_model().to_string()
    } else {
        format!(
            "{} · {} → {}",
            app.shared_mode(),
            app.shared_profile(),
            app.active_model()
        )
    };
    let context = if app.has_stashed_draft() {
        format!("stashed · {context}")
    } else {
        context
    };
    let busy = matches!(app.active_work(), crate::WorkState::Busy { .. });
    let theme = theme(app);
    let text = if narrow {
        let shortcuts = if busy {
            if app.is_pair() {
                "F6 peer"
            } else {
                ""
            }
        } else {
            shortcuts
        };
        format!(
            "{}\n{}",
            truncate_end(&state, area.width),
            two_sided(shortcuts, &context, area.width)
        )
    } else {
        let left = if busy {
            if app.is_pair() {
                format!("{state} · F6 peer")
            } else {
                state.clone()
            }
        } else {
            format!("{state} · {shortcuts}")
        };
        two_sided(&left, &context, area.width)
    };
    frame.render_widget(Paragraph::new(text).style(theme.ui(UiRole::Muted)), area);
    if let Some(activity) = activity {
        let Some(offset) = state.find(&activity) else {
            return;
        };
        let x = area
            .x
            .saturating_add(u16::try_from(UnicodeWidthStr::width(&state[..offset])).unwrap_or(0));
        let width = u16::try_from(UnicodeWidthStr::width(activity.as_str())).unwrap_or(u16::MAX);
        frame.render_widget(
            Paragraph::new(activity).style(theme.ui(UiRole::Accent)),
            Rect::new(x, area.y, width.min(area.right().saturating_sub(x)), 1),
        );
    }
}

fn working_status(app: &AppModel) -> Option<String> {
    let (label, elapsed) = app.active_work_activity()?;
    Some(format!(
        "{} {label} · {}",
        working_glyph(elapsed),
        format_elapsed(elapsed)
    ))
}

fn working_glyph(elapsed: Duration) -> &'static str {
    const FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];
    let frame = usize::try_from(elapsed.as_millis() / 200).unwrap_or(usize::MAX) % FRAMES.len();
    FRAMES[frame]
}

fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    if minutes < 60 {
        format!("{minutes:02}:{seconds:02}")
    } else {
        let hours = minutes / 60;
        let minutes = minutes % 60;
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    }
}

#[cfg(test)]
fn footer_state(app: &AppModel) -> String {
    let activity = working_status(app);
    footer_state_with_activity(app, activity.as_deref())
}

fn footer_state_with_activity(app: &AppModel, activity: Option<&str>) -> String {
    let held = !matches!(
        app.active_timeline().viewport,
        crate::ViewportAnchor::FollowBottom
    );
    let new_output = app.active_timeline().has_new_content();
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
    if app.active_timeline().selected_text().is_some() {
        return if app.capabilities.clipboard_write {
            "selection · Ctrl+C / right-click copy".to_string()
        } else {
            "selection · clipboard unavailable".to_string()
        };
    }
    if app.shell_mode() {
        return "shell mode · Esc exit shell mode".to_string();
    }
    match (app.active_work(), app.exit_armed) {
        (_, true) => "press Ctrl+C again to exit".to_string(),
        (crate::WorkState::Idle, false) if held && new_output => {
            if app.editor.text().is_empty() {
                "↓ new output · timeline held · Ctrl+C twice to exit".to_string()
            } else {
                "↓ new output · timeline held · Ctrl+C clear draft".to_string()
            }
        }
        (crate::WorkState::Idle, false) if held => {
            if app.editor.text().is_empty() {
                "timeline held · Ctrl+C twice to exit".to_string()
            } else {
                "timeline held · Ctrl+C clear draft".to_string()
            }
        }
        (crate::WorkState::Idle, false) if !app.editor.text().is_empty() => {
            "idle · Ctrl+C clear draft".to_string()
        }
        (crate::WorkState::Idle, false) => "idle · Ctrl+C copy / twice to exit".to_string(),
        (
            crate::WorkState::Busy {
                cancellation_requested: false,
            },
            false,
        ) if held && new_output => {
            format!(
                "↓ new output · {} · {} · {}",
                activity.unwrap_or("Working · 00:00"),
                format_stream_size(app.active_stream_bytes()),
                working_input_actions(app)
            )
        }
        (
            crate::WorkState::Busy {
                cancellation_requested: false,
            },
            false,
        ) if held => format!(
            "timeline held · {} · {} · {}",
            activity.unwrap_or("Working · 00:00"),
            format_stream_size(app.active_stream_bytes()),
            working_input_actions(app)
        ),
        (
            crate::WorkState::Busy {
                cancellation_requested: false,
            },
            false,
        ) => format!(
            "{} · {} · {}",
            activity.unwrap_or("Working · 00:00"),
            format_stream_size(app.active_stream_bytes()),
            working_input_actions(app)
        ),
        (
            crate::WorkState::Busy {
                cancellation_requested: true,
            },
            false,
        ) => format!(
            "{} · Ctrl+C twice to exit",
            activity
                .map(|status| status.replacen("Working", "Cancelling", 1))
                .unwrap_or_else(|| "Cancelling · 00:00".to_string())
        ),
    }
}

fn working_input_actions(app: &AppModel) -> &'static str {
    if app.is_pair() {
        // A collaboration has no single-turn interrupt; the control is a full abort.
        return "/abort stops both peers";
    }
    if app.editor.text().is_empty() {
        "Ctrl+C / Esc interrupt"
    } else {
        "Ctrl+C clear · Esc interrupt · Ctrl+Q enqueue"
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

    fn single_timeline(layout: FrameLayout) -> TimelinePaneLayout {
        let TimelineLayout::Single(timeline) = layout.timeline else {
            panic!("ordinary render must use a single timeline")
        };
        timeline
    }
    use crate::{ColorSupport, Header, ItemKind, TerminalCapabilities, Theme};

    fn header() -> Header {
        Header {
            version: "0".to_string(),
            provider: "provider".to_string(),
            model: "model".to_string(),
            workspace: "workspace".to_string(),
            branch: Some("main".to_string()),
            workspace_dirty: Some(false),
            mode: localpilot_slash::Mode::Agent,
            profile: "default".to_string(),
            session_id: "session".to_string(),
            session_name: None,
        }
    }

    fn pair_model() -> AppModel {
        AppModel::new_pair(
            header(),
            crate::SessionHeader {
                provider: "provider-b".to_string(),
                model: "model-b".to_string(),
                session_id: "session-b".to_string(),
                session_name: Some("Beta".to_string()),
            },
            TerminalCapabilities::default(),
        )
    }

    fn snapshot_header() -> Header {
        Header {
            version: "snapshot-version".to_string(),
            provider: "snapshot-provider".to_string(),
            model: "snapshot-model".to_string(),
            workspace: "snapshot-workspace".to_string(),
            branch: Some("snapshot-branch".to_string()),
            workspace_dirty: Some(false),
            mode: localpilot_slash::Mode::Agent,
            profile: "snapshot-profile".to_string(),
            session_id: "snapshot-session-a".to_string(),
            session_name: Some("Snapshot Alpha".to_string()),
        }
    }

    fn model() -> AppModel {
        AppModel::new(header(), TerminalCapabilities::default())
    }

    fn single_hits(hit_map: &HitMap) -> &TimelinePaneHits {
        hit_map
            .timelines
            .as_ref()
            .and_then(|timelines| timelines.active(None))
            .expect("single timeline hits")
    }

    fn rect_text(buffer: &Buffer, area: Rect) -> String {
        let mut text = String::new();
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    // These snapshots intentionally freeze character cells and geometry only.
    // Focus and muted styles stay covered by targeted cell-style assertions.
    fn character_cell_snapshot(buffer: &Buffer) -> String {
        let mut snapshot = String::new();
        for y in buffer.area.y..buffer.area.bottom() {
            for x in buffer.area.x..buffer.area.right() {
                match buffer[(x, y)].symbol() {
                    "" => snapshot.push('∅'),
                    " " => snapshot.push('␠'),
                    symbol => snapshot.push_str(symbol),
                }
            }
            snapshot.push('\n');
        }
        snapshot
    }

    fn render_test_frame(app: &AppModel, width: u16, height: u16) -> (Buffer, HitMap) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, app)))
            .expect("draw test frame");
        (
            terminal.backend().buffer().clone(),
            hit_map.expect("test hit map"),
        )
    }

    fn add_snapshot_peer_turn(app: &mut AppModel, peer: PeerPane, marker: &str, turn: usize) {
        let _ = app
            .timeline_for_mut(peer)
            .expect("named peer timeline")
            .push(
                ItemKind::User,
                format!("{marker}_PROMPT_{turn:02} inspect the requested module"),
            );
        assert!(app.apply_runtime_for(
            peer,
            crate::RuntimeUpdate::Reasoning(format!(
                "{marker}_REASONING_{turn:02} checking context"
            )),
        ));
        if turn == 4 {
            let id = format!("snapshot-{marker}-{turn}");
            let name = format!("{marker}_TOOL");
            assert!(app.apply_runtime_for(
                peer,
                crate::RuntimeUpdate::ToolStarted {
                    id: id.clone(),
                    name: name.clone(),
                    detail: format!("{marker}_TARGET.rs"),
                },
            ));
            assert!(app.apply_runtime_for(
                peer,
                crate::RuntimeUpdate::ToolFinished {
                    id,
                    name,
                    is_error: false,
                    cancelled: false,
                    output: format!("{marker}_OUTPUT_ONE\n{marker}_OUTPUT_TWO\n"),
                    duration_ms: 250,
                },
            ));
            // This pair/resize golden deliberately keeps a fixed one-row Tool
            // sentinel. Compact result previews have focused wide/narrow and
            // accessibility coverage below; including them here would make an
            // unrelated peer-geometry fixture depend on result-row budgeting.
            let timeline = app.timeline_for_mut(peer).expect("named peer timeline");
            let tool = timeline
                .items()
                .iter()
                .rfind(|item| item.kind == ItemKind::Tool)
                .map(|item| item.id)
                .expect("snapshot tool");
            assert!(
                timeline.replace_text(tool, format!("{marker}_TOOL completed · 2 lines · 250 ms"))
            );
        }
        assert!(app.apply_runtime_for(
            peer,
            crate::RuntimeUpdate::Text(format!(
                "{marker}_ANSWER_{turn:02} completed the requested review"
            )),
        ));
        assert!(app.apply_runtime_for(peer, crate::RuntimeUpdate::Stopped(crate::StopState::Done),));
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
        let timeline = single_hits(&hit_map);
        let timelines = hit_map.timelines.as_ref().expect("visible timeline");
        assert!(timelines.for_peer(PeerPane::A).is_none());
        assert!(timelines.for_peer(PeerPane::B).is_none());
        assert_eq!(hit_map.tabs.len(), 2);
        assert_eq!(hit_map.tabs[0].tab, TabId::Session);
        assert_eq!(hit_map.tabs[1].tab, TabId::LocalMind);
        assert!(timeline.timeline.height > 0);
        assert!(hit_map.composer.height > 0);
        assert_eq!(
            timeline.timeline.right().saturating_add(1),
            timeline.scrollbar.track.x,
            "timeline wrapping must leave one blank cell before the scrollbar"
        );
        let layout = hit_map.frame.expect("frame layout");
        assert_eq!(layout.status.x, timeline.timeline.x);
        assert_eq!(layout.footer.x, timeline.timeline.x);
        assert_eq!(
            terminal.backend().buffer()[(timeline.timeline.right(), timeline.timeline.y)].symbol(),
            " "
        );
    }

    #[test]
    fn pair_rendering_keeps_named_content_and_hit_geometry_isolated() {
        let mut app = AppModel::new_pair(
            header(),
            crate::SessionHeader {
                provider: "provider-b\nnext".to_string(),
                model: "model-b\tvariant".to_string(),
                session_id: "session-b".to_string(),
                session_name: Some("Beta".to_string()),
            },
            TerminalCapabilities::default(),
        );
        for index in 0..24 {
            app.apply_runtime(crate::RuntimeUpdate::Text(format!(
                "ALPHA_ONLY_{index:02}\n"
            )));
            assert!(app.apply_runtime_for(
                PeerPane::B,
                crate::RuntimeUpdate::Text(format!("BETA_ONLY_{index:02}\n"))
            ));
        }

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw pair");
        let hit_map = hit_map.expect("pair hit map");
        let timelines = hit_map.timelines.as_ref().expect("visible timelines");
        let a = timelines.for_peer(PeerPane::A).expect("peer A hits");
        let b = timelines.for_peer(PeerPane::B).expect("peer B hits");
        assert_eq!(a.peer, Some(PeerPane::A));
        assert_eq!(b.peer, Some(PeerPane::B));
        assert_eq!(timelines.active(Some(PeerPane::A)), Some(a));
        assert_eq!(timelines.active(Some(PeerPane::B)), Some(b));
        assert!(timelines.active(None).is_none());
        let TimelineLayout::Pair(layout) = hit_map.frame.expect("frame").timeline else {
            panic!("wide pair layout")
        };
        assert_eq!(a.label, layout.first.label);
        assert_eq!(a.viewport, layout.first.viewport);
        assert_eq!(a.timeline, layout.first.content);
        assert_eq!(a.scrollbar.track, layout.first.scrollbar);
        assert_eq!(b.label, layout.second.label);
        assert_eq!(b.viewport, layout.second.viewport);
        assert_eq!(b.timeline, layout.second.content);
        assert_eq!(b.scrollbar.track, layout.second.scrollbar);

        let a_area = Rect::new(
            a.viewport.x,
            a.label.map_or(a.viewport.y, |label| label.y),
            a.viewport.width,
            a.viewport
                .height
                .saturating_add(u16::from(a.label.is_some())),
        );
        let b_area = Rect::new(
            b.viewport.x,
            b.label.map_or(b.viewport.y, |label| label.y),
            b.viewport.width,
            b.viewport
                .height
                .saturating_add(u16::from(b.label.is_some())),
        );
        let a_text = rect_text(terminal.backend().buffer(), a_area);
        let b_text = rect_text(terminal.backend().buffer(), b_area);
        assert!(a_text.contains("Peer A [active] · provider · model"));
        assert!(a_text.contains("ALPHA_ONLY"));
        assert!(!a_text.contains("BETA_ONLY"));
        assert!(b_text.contains("Peer B · provider-b next · model-b variant"));
        assert!(b_text.contains("BETA_ONLY"));
        assert!(!b_text.contains("ALPHA_ONLY"));
        assert!(terminal.backend().to_string().contains("F6 peer"));
        assert_eq!(
            timelines
                .at(a.timeline.x, a.timeline.y)
                .and_then(|hits| hits.peer),
            Some(PeerPane::A)
        );
        let a_label = a.label.expect("peer A label");
        assert_eq!(
            terminal.backend().buffer()[(a_label.x, a_label.y)]
                .style()
                .fg,
            theme(&app).ui(UiRole::Focus).fg
        );
        assert_eq!(
            timelines
                .at(a_label.x, a_label.y)
                .and_then(|hits| hits.peer),
            Some(PeerPane::A)
        );
        assert_eq!(
            timelines
                .at(b.timeline.x, b.timeline.y)
                .and_then(|hits| hits.peer),
            Some(PeerPane::B)
        );
        let divider = timelines.divider().expect("pair divider");
        assert!(timelines.at(divider.x, divider.y).is_none());
        assert!(timelines.at(0, hit_map.composer.y).is_none());

        let a_rows = a.rows.len();
        let b_rows = b.rows.len();
        assert!(app.select_pair_pane(PeerPane::B));
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw pair focused B");
        let focused = terminal.backend().to_string();
        assert!(focused.contains("Peer A · provider · model"));
        assert!(focused.contains("Peer B [active] · provider-b next"));
        assert_eq!(
            terminal.backend().buffer()[(a_label.x, a_label.y)]
                .style()
                .fg,
            theme(&app).ui(UiRole::Muted).fg
        );
        assert!(app.select_pair_pane(PeerPane::A));
        app.begin_work();
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw busy pair footer");
        assert!(terminal.backend().to_string().contains("F6 peer"));
        app.apply_runtime(crate::RuntimeUpdate::Stopped(crate::StopState::Done));
        let _ = app.handle_input(crate::InputAction::Insert("?".to_string()), 46);
        let mut quick_hit_map = None;
        terminal
            .draw(|frame| quick_hit_map = Some(render(frame, &app)))
            .expect("draw pair quick help");
        let hit_map = quick_hit_map.expect("pair quick-help hit map");
        let timelines = hit_map.timelines.as_ref().expect("visible timelines");
        assert!(timelines.for_peer(PeerPane::A).expect("A").rows.len() < a_rows);
        assert_eq!(
            timelines.for_peer(PeerPane::B).expect("B").rows.len(),
            b_rows
        );
        assert!(terminal
            .backend()
            .to_string()
            .contains("F6          switch peer"));
    }

    #[test]
    fn peer_frame_snapshot_and_resize_round_trip_preserve_focus_scroll_search_selection_and_draft()
    {
        let mut app = AppModel::new_pair(
            snapshot_header(),
            crate::SessionHeader {
                provider: "snapshot-provider-b".to_string(),
                model: "snapshot-model-b".to_string(),
                session_id: "snapshot-session-b".to_string(),
                session_name: Some("Snapshot Beta".to_string()),
            },
            TerminalCapabilities::default(),
        );
        for turn in 0..14 {
            add_snapshot_peer_turn(&mut app, PeerPane::A, "ALPHA", turn);
            add_snapshot_peer_turn(&mut app, PeerPane::B, "BETA", turn);
        }

        let (_, initial_hits) = render_test_frame(&app, 120, 30);
        let timelines = initial_hits.timelines.as_ref().expect("wide timelines");
        let initial_a = timelines.for_peer(PeerPane::A).expect("peer A").clone();
        let initial_b = timelines.for_peer(PeerPane::B).expect("peer B").clone();
        app.timeline_for_mut(PeerPane::A)
            .expect("peer A timeline")
            .scroll_to_row(6, initial_a.timeline.width, initial_a.timeline.height);
        app.timeline_for_mut(PeerPane::B)
            .expect("peer B timeline")
            .scroll_to_row(19, initial_b.timeline.width, initial_b.timeline.height);
        let starts_before_focus = (
            app.timeline_for(PeerPane::A)
                .expect("peer A timeline")
                .view(initial_a.timeline.width, initial_a.timeline.height)
                .start,
            app.timeline_for(PeerPane::B)
                .expect("peer B timeline")
                .view(initial_b.timeline.width, initial_b.timeline.height)
                .start,
        );
        assert_eq!(starts_before_focus, (6, 19));
        assert_eq!(
            app.handle_input(crate::InputAction::CyclePeer, initial_hits.composer.width),
            crate::AppCommand::None
        );
        assert_eq!(app.active_pair_pane(), Some(PeerPane::B));

        let (wide_buffer, wide_hits) = render_test_frame(&app, 120, 30);
        let wide_timelines = wide_hits.timelines.as_ref().expect("wide timelines");
        let wide_a = wide_timelines.for_peer(PeerPane::A).expect("peer A");
        let wide_b = wide_timelines.for_peer(PeerPane::B).expect("peer B");
        assert_eq!(wide_a.peer, Some(PeerPane::A));
        assert_eq!(wide_b.peer, Some(PeerPane::B));
        assert_eq!(wide_a.timeline, initial_a.timeline);
        assert_eq!(wide_b.timeline, initial_b.timeline);
        let starts_after_focus = (
            app.timeline_for(PeerPane::A)
                .expect("peer A timeline")
                .view(wide_a.timeline.width, wide_a.timeline.height)
                .start,
            app.timeline_for(PeerPane::B)
                .expect("peer B timeline")
                .view(wide_b.timeline.width, wide_b.timeline.height)
                .start,
        );
        assert_eq!(starts_after_focus, starts_before_focus);
        let wide_a_text = rect_text(&wide_buffer, wide_a.timeline);
        let wide_b_text = rect_text(&wide_buffer, wide_b.timeline);
        assert!(wide_a_text.contains("ALPHA_"));
        assert!(!wide_a_text.contains("BETA_"));
        assert!(wide_b_text.contains("BETA_"));
        assert!(!wide_b_text.contains("ALPHA_"));
        let wide_text = rect_text(&wide_buffer, wide_buffer.area);
        assert!(wide_text.contains("Peer B [active]"));
        assert!(!wide_text.contains("Peer A [active]"));

        let (narrow_buffer, narrow_hits) = render_test_frame(&app, 60, 24);
        let narrow_timelines = narrow_hits.timelines.as_ref().expect("narrow timeline");
        assert!(narrow_timelines.for_peer(PeerPane::A).is_none());
        assert_eq!(
            narrow_timelines
                .for_peer(PeerPane::B)
                .and_then(|timeline| timeline.peer),
            Some(PeerPane::B)
        );
        let narrow_text = rect_text(&narrow_buffer, narrow_buffer.area);
        assert!(narrow_text.contains("Peer B [active]"));
        assert!(!narrow_text.contains("Peer A"));

        let wide_snapshot = character_cell_snapshot(&wide_buffer);
        let narrow_snapshot = character_cell_snapshot(&narrow_buffer);
        assert_eq!(
            wide_snapshot,
            include_str!("fixtures/peer_wide_120x30.cells")
        );
        assert_eq!(
            narrow_snapshot,
            include_str!("fixtures/peer_narrow_60x24.cells")
        );

        // --- Augmented resize round trip -------------------------------------
        // The wide/narrow goldens above are captured at the base state (scroll
        // 6/19, B active, no search or selection), so the augmentation below
        // cannot disturb them. It layers the remaining geometry-independent
        // state — an A-owned parked timeline search, a selection inside B's
        // active timeline, and a shared composer draft — and asserts every piece
        // survives a full wide->narrow->wide resize.
        let a_rect = initial_a.timeline;
        let b_rect = initial_b.timeline;

        // Establish A's real timeline search through the public input path while
        // A is active (empty composer -> ForwardCharOrSearch opens the search,
        // Insert types the query), then park it by returning focus to B.
        assert!(app.select_pair_pane(PeerPane::A));
        let _ = app.handle_input(crate::InputAction::ForwardCharOrSearch, a_rect.width);
        let _ = app.handle_input(
            crate::InputAction::Insert("ALPHA".to_string()),
            a_rect.width,
        );
        assert_eq!(
            app.timeline_search().expect("A search opened").query,
            "ALPHA"
        );
        assert!(app.select_pair_pane(PeerPane::B));
        assert!(
            app.timeline_search().is_none(),
            "A's search parks when B is active"
        );

        // A selection inside B's active timeline and a shared composer draft;
        // with B active the composer targets Steer Peer B.
        let (b_item, b_len) = {
            let item = app
                .timeline_for(PeerPane::B)
                .unwrap()
                .items()
                .iter()
                .find(|item| item.kind == ItemKind::User)
                .expect("a B user row");
            (item.id, item.text.len())
        };
        {
            let timeline = app.timeline_for_mut(PeerPane::B).unwrap();
            timeline.start_selection(crate::ContentPoint {
                item_id: b_item,
                byte: 0,
            });
            timeline.extend_selection(crate::ContentPoint {
                item_id: b_item,
                byte: b_len,
            });
        }
        app.editor.replace_draft("shared draft in flight");

        // Capture the augmented, geometry-independent state the resize must keep.
        let state = |app: &AppModel| {
            (
                app.active_pair_pane(),
                app.timeline_for(PeerPane::A)
                    .unwrap()
                    .view(a_rect.width, a_rect.height)
                    .start,
                app.timeline_for(PeerPane::B)
                    .unwrap()
                    .view(b_rect.width, b_rect.height)
                    .start,
                app.editor.text().to_string(),
                app.timeline_for(PeerPane::B).unwrap().selected_text(),
            )
        };
        let before = state(&app);
        assert_eq!(before.0, Some(PeerPane::B));
        assert_eq!(before.3, "shared draft in flight");
        assert!(before.4.is_some(), "B selection is present");

        // Wide: both hit bundles present, composer targets Steer Peer B.
        let (rt_wide, rt_wide_hits) = render_test_frame(&app, 120, 30);
        let rt_wb = rt_wide_hits.timelines.as_ref().unwrap();
        assert!(rt_wb.for_peer(PeerPane::A).is_some() && rt_wb.for_peer(PeerPane::B).is_some());
        assert!(rect_text(&rt_wide, rt_wide.area).contains("Steer Peer B"));

        // Narrow: only the active B bundle; A carries no stale off-screen hit map.
        let (rt_narrow, rt_narrow_hits) = render_test_frame(&app, 60, 24);
        let rt_nb = rt_narrow_hits.timelines.as_ref().unwrap();
        assert!(
            rt_nb.for_peer(PeerPane::A).is_none(),
            "no stale A hits when narrow"
        );
        assert_eq!(
            rt_nb
                .for_peer(PeerPane::B)
                .and_then(|timeline| timeline.peer),
            Some(PeerPane::B)
        );
        assert!(rect_text(&rt_narrow, rt_narrow.area).contains("Peer B [active]"));

        // Back to wide: both bundles return and the whole captured state is
        // state-identical (typed state, not bytes).
        let (rt_wide_again, rt_wide_again_hits) = render_test_frame(&app, 120, 30);
        let rt_wa = rt_wide_again_hits.timelines.as_ref().unwrap();
        assert!(rt_wa.for_peer(PeerPane::A).is_some() && rt_wa.for_peer(PeerPane::B).is_some());
        assert_eq!(
            state(&app),
            before,
            "typed state survives wide->narrow->wide"
        );
        assert!(rect_text(&rt_wide_again, rt_wide_again.area).contains("Steer Peer B"));

        // The A-owned search resumes on returning focus to A; B's shared draft,
        // selection, and viewports remain intact on switching back.
        assert!(app.select_pair_pane(PeerPane::A));
        assert_eq!(
            app.timeline_search().expect("A search resumes").query,
            "ALPHA"
        );
        assert!(app.select_pair_pane(PeerPane::B));
        assert_eq!(state(&app), before, "B state intact after visiting A");
    }

    #[test]
    fn narrow_pair_reports_only_the_active_named_pane() {
        let mut app = AppModel::new_pair(
            header(),
            crate::SessionHeader {
                provider: "provider-b".to_string(),
                model: "model-b".to_string(),
                session_id: "session-b".to_string(),
                session_name: None,
            },
            TerminalCapabilities::default(),
        );
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).expect("terminal");
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw narrow pair");
        let hit_map = hit_map.expect("narrow pair hit map");
        let timelines = hit_map.timelines.as_ref().expect("visible timeline");
        let active = timelines
            .active(Some(PeerPane::A))
            .expect("active peer A hits");
        assert_eq!(active.peer, Some(PeerPane::A));
        assert!(active.label.is_some());
        assert_eq!(timelines.for_peer(PeerPane::A), Some(active));
        assert!(timelines.for_peer(PeerPane::B).is_none());
        assert!(timelines.active(None).is_none());

        assert_eq!(
            app.handle_input(crate::InputAction::CyclePeer, 58),
            crate::AppCommand::None
        );
        let mut b_hit_map = None;
        terminal
            .draw(|frame| b_hit_map = Some(render(frame, &app)))
            .expect("draw narrow peer B");
        let b_hit_map = b_hit_map.expect("narrow B hit map");
        let timelines = b_hit_map.timelines.as_ref().expect("visible B timeline");
        assert!(timelines.for_peer(PeerPane::A).is_none());
        assert_eq!(
            timelines
                .active(Some(PeerPane::B))
                .and_then(|timeline| timeline.peer),
            Some(PeerPane::B)
        );
        assert!(terminal.backend().to_string().contains("Peer B [active]"));
        assert!(app.cycle_pair_pane());
        assert_eq!(app.active_pair_pane(), Some(PeerPane::A));
    }

    #[test]
    fn pair_dialog_headings_name_the_requesting_peer_for_both_render_modes() {
        let secondary = crate::SessionHeader {
            provider: "provider-b".to_string(),
            model: "model-b".to_string(),
            session_id: "session-b".to_string(),
            session_name: None,
        };
        let mut app =
            AppModel::new_pair(header(), secondary.clone(), TerminalCapabilities::default());
        assert!(app.request_approval_for(PeerPane::B, "write", "target", "ask"));
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw approval");
        assert!(terminal
            .backend()
            .to_string()
            .contains("Peer B · model-b · Permission required"));

        let mut screen_reader = AppModel::new_pair(
            header(),
            secondary,
            TerminalCapabilities {
                screen_reader: true,
                ..TerminalCapabilities::default()
            },
        );
        assert!(screen_reader.request_question_for(
            PeerPane::B,
            None,
            "Continue?",
            [
                crate::QuestionOption {
                    label: "Yes".to_string(),
                    description: None,
                },
                crate::QuestionOption {
                    label: "No".to_string(),
                    description: None,
                },
            ],
            false,
            1,
            1,
        ));
        terminal
            .draw(|frame| {
                let _ = render(frame, &screen_reader);
            })
            .expect("draw screen-reader question");
        let rendered = terminal.backend().to_string();
        assert!(
            rendered.contains("Peer B · model-b · Question"),
            "{rendered}"
        );
    }

    #[test]
    fn pair_dialog_footers_keep_both_safety_controls_visible_after_width_handling() {
        let secondary = || crate::SessionHeader {
            provider: "provider-b".to_string(),
            model: "model-b".to_string(),
            session_id: "session-b".to_string(),
            session_name: None,
        };
        let options = || {
            [
                crate::QuestionOption {
                    label: "Yes".to_string(),
                    description: None,
                },
                crate::QuestionOption {
                    label: "No".to_string(),
                    description: None,
                },
            ]
        };
        let rendered = |app: &AppModel, width: u16, height: u16| {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
            terminal
                .draw(|frame| {
                    let _ = render(frame, app);
                })
                .expect("draw dialog");
            terminal.backend().to_string()
        };
        let has_both = |text: &str, abort: bool, esc: &str| {
            assert_eq!(text.contains("Ctrl+C abort"), abort, "abort in: {text}");
            if abort {
                assert!(text.contains(esc), "`{esc}` missing in: {text}");
            }
        };

        // Normal approval keeps both the abort and the Esc-deny controls.
        let mut approval =
            AppModel::new_pair(header(), secondary(), TerminalCapabilities::default());
        assert!(approval.request_approval_for(PeerPane::B, "write", "target", "ask"));
        has_both(&rendered(&approval, 60, 24), true, "Esc deny");

        // Screen-reader approval keeps both.
        let mut sr_approval = AppModel::new_pair(
            header(),
            secondary(),
            TerminalCapabilities {
                screen_reader: true,
                ..TerminalCapabilities::default()
            },
        );
        assert!(sr_approval.request_approval_for(PeerPane::B, "write", "target", "ask"));
        has_both(&rendered(&sr_approval, 60, 24), true, "Esc deny");

        // A narrow normal question truncates its footer, but both controls lead and
        // survive width handling.
        let mut question =
            AppModel::new_pair(header(), secondary(), TerminalCapabilities::default());
        assert!(question.request_question_for(
            PeerPane::B,
            None,
            "Continue with the shared plan right now?",
            options(),
            false,
            1,
            1,
        ));
        has_both(&rendered(&question, 48, 20), true, "Esc dismiss");

        // Screen-reader question keeps both.
        let mut sr_question = AppModel::new_pair(
            header(),
            secondary(),
            TerminalCapabilities {
                screen_reader: true,
                ..TerminalCapabilities::default()
            },
        );
        assert!(sr_question.request_question_for(
            PeerPane::B,
            None,
            "Continue?",
            options(),
            false,
            1,
            1,
        ));
        has_both(&rendered(&sr_question, 60, 24), true, "Esc dismiss");

        // Editing the Other field still shows both the abort and the Esc-choices
        // controls. Navigate onto the implicit Other option and submit to enter it.
        let mut other = AppModel::new_pair(header(), secondary(), TerminalCapabilities::default());
        assert!(other.request_question_for(PeerPane::B, None, "Pick one", options(), false, 1, 1));
        let _ = other.handle_question_input(crate::InputAction::MoveDown);
        let _ = other.handle_question_input(crate::InputAction::MoveDown);
        assert!(matches!(
            other.handle_question_input(crate::InputAction::Submit),
            crate::QuestionAction::None
        ));
        has_both(&rendered(&other, 60, 20), true, "Esc choices");

        // Single chat keeps its plain footer with no pair abort control.
        let mut single = model();
        single.request_approval("write", "target", "ask");
        has_both(&rendered(&single, 60, 24), false, "");
    }

    #[test]
    fn a_pair_dialog_heading_sanitizes_the_model_and_falls_back_when_empty() {
        // A malicious/control-laden model label is sanitized into the heading.
        let noisy = crate::SessionHeader {
            provider: "provider-b".to_string(),
            model: "evil\nmodel\u{7}x".to_string(),
            session_id: "session-b".to_string(),
            session_name: None,
        };
        let mut app = AppModel::new_pair(header(), noisy, TerminalCapabilities::default());
        assert!(app.request_approval_for(PeerPane::B, "write", "target", "ask"));
        let heading = dialog_heading(&app, "Permission required");
        assert!(heading.starts_with("Peer B · "));
        assert!(heading.ends_with(" · Permission required"));
        assert!(!heading.contains('\n') && !heading.contains('\u{7}'));

        // An empty model label falls back to an explicit placeholder, never a blank.
        let blank = crate::SessionHeader {
            provider: "provider-b".to_string(),
            model: String::new(),
            session_id: "session-b".to_string(),
            session_name: None,
        };
        let mut app = AppModel::new_pair(header(), blank, TerminalCapabilities::default());
        assert!(app.request_approval_for(PeerPane::B, "write", "target", "ask"));
        assert_eq!(
            dialog_heading(&app, "Permission required"),
            "Peer B · unknown model · Permission required"
        );

        // Single chat keeps a plain heading with no peer or model.
        let mut single = model();
        single.request_approval("write", "target", "ask");
        assert_eq!(
            dialog_heading(&single, "Permission required"),
            "Permission required"
        );
    }

    #[test]
    fn a_running_collaboration_footer_uses_abort_language_not_interrupt() {
        let pair = AppModel::new_pair(
            snapshot_header(),
            crate::SessionHeader {
                provider: "provider-b".to_string(),
                model: "model-b".to_string(),
                session_id: "session-b".to_string(),
                session_name: None,
            },
            TerminalCapabilities::default(),
        );
        assert_eq!(working_input_actions(&pair), "/abort stops both peers");
        // Single chat keeps its exact single-turn interrupt copy.
        let single = model();
        assert_eq!(working_input_actions(&single), "Ctrl+C / Esc interrupt");
    }

    #[test]
    fn help_takeover_replaces_chat_chrome_and_owns_its_scrollbar() {
        let mut app = model();
        let _ = app
            .active_timeline_mut()
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
        assert!(hit_map.takeover_scrollbar.thumb.is_some());
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
                edit: None,
                is_default: true,
            },
            crate::SettingEntry {
                section: "Appearance".into(),
                name: "Color mode".into(),
                value: "Default".into(),
                description: "Semantic terminal colors".into(),
                edit: Some(crate::SettingEdit::Theme),
                is_default: true,
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
        assert!(rendered.contains("2 items"));
        assert!(rendered.contains("Appearance"));
        assert!(rendered.contains("Color mode"));
        assert!(rendered.contains("Semantic terminal colors"));
        assert!(rendered.contains("Search: type to filter"));
        assert!(rendered.contains("Enter edit"));
        assert_eq!(hit_map.takeover_rows.len(), 2);
        assert_eq!(hit_map.takeover_rows[1].index, 1);
    }

    #[test]
    fn settings_takeover_has_compact_screen_reader_and_empty_search_states() {
        for screen_reader in [false, true] {
            let mut app = model();
            app.capabilities.screen_reader = screen_reader;
            app.open_settings([
                crate::SettingEntry {
                    section: "Input".into(),
                    name: "Mouse reporting".into(),
                    value: "On".into(),
                    description: "Capture pointer events".into(),
                    edit: None,
                    is_default: true,
                },
                crate::SettingEntry {
                    section: "Appearance".into(),
                    name: "Color mode".into(),
                    value: "Default".into(),
                    description: "Semantic terminal colors".into(),
                    edit: Some(crate::SettingEdit::Theme),
                    is_default: true,
                },
            ]);
            let mut terminal = Terminal::new(TestBackend::new(30, 10)).expect("terminal");
            let mut hit_map = None;
            terminal
                .draw(|frame| hit_map = Some(render(frame, &app)))
                .expect("draw compact settings");
            let rendered = terminal.backend().to_string();
            assert!(rendered.contains("2 items"));
            assert!(rendered.contains("Input · Mouse reporting"));
            assert!(rendered.contains("Search: type to filter"));
            assert!(!rendered.contains("Enter edit"));
            assert_eq!(hit_map.expect("settings hits").takeover_rows.len(), 2);
            if screen_reader {
                assert!(rendered.contains("Current selection"));
            }

            let _ = app.handle_input(crate::InputAction::Insert("no-match".into()), 25);
            terminal
                .draw(|frame| {
                    let _ = render(frame, &app);
                })
                .expect("draw empty settings search");
            let rendered = terminal.backend().to_string();
            assert!(rendered.contains("0 items"));
            assert!(rendered.contains("No settings match"));
            assert!(rendered.contains("Search: no-match"));
            assert!(rendered.contains("Esc clear"));
        }
    }

    #[test]
    fn sessions_takeover_renders_current_selection_actions_and_click_targets() {
        for (width, height, screen_reader) in [(120, 30, false), (30, 10, false), (30, 10, true)] {
            let mut app = model();
            app.capabilities.screen_reader = screen_reader;
            app.open_sessions([
                crate::SessionEntry {
                    selector: "11111111-1111-1111-1111-111111111111".into(),
                    name: Some("Current work".into()),
                    message_count: 12,
                    updated: Some("2026-08-01 10:00".into()),
                    current: true,
                },
                crate::SessionEntry {
                    selector: "22222222-2222-2222-2222-222222222222".into(),
                    name: Some("Earlier work".into()),
                    message_count: 4,
                    updated: Some("2026-07-31 18:30".into()),
                    current: false,
                },
            ]);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
            let mut hit_map = None;
            terminal
                .draw(|frame| hit_map = Some(render(frame, &app)))
                .expect("draw sessions");

            let rendered = terminal.backend().to_string();
            let hit_map = hit_map.expect("session hit map");
            assert!(rendered.contains("Sessions"));
            assert!(rendered.contains("Current selection"));
            assert!(rendered.contains("Enter resume"));
            assert!(rendered.contains("Esc return"));
            assert!(!rendered.contains("resize to view help"));
            assert_eq!(hit_map.takeover_rows.len(), 2);
            assert_eq!(hit_map.takeover_rows[0].index, 0);
            assert_eq!(hit_map.takeover_rows[0].area.width, width.saturating_sub(5));
            if screen_reader {
                assert!(hit_map.takeover_scrollbar.thumb.is_none());
            }
        }
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
        let deletion = hit_map.takeover_rows[1].area;
        assert_eq!(
            terminal.backend().buffer()[(deletion.x, deletion.y)].style(),
            ThemeResolver::new(Theme::Default, ColorSupport::Color).ui(UiRole::DiffDeletion)
        );
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
        assert_eq!(hit_map.takeover_scrollbar.total_rows, 20);
        assert!(hit_map.takeover_scrollbar.thumb.is_some());
    }

    #[test]
    fn quick_help_is_a_two_column_timeline_overlay() {
        let mut app = model();
        for number in 0..20 {
            let _ = app
                .active_timeline_mut()
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
        assert!(rendered.contains("Ctrl+S      stash / restore draft"));
        assert!(rendered.contains("Esc Esc      clear draft"));
        assert!(footer_state(&app).contains("? or Esc close"));
        let timeline = single_hits(&hit_map);
        assert!(timeline.rows.len() < usize::from(timeline.timeline.height));
    }

    #[test]
    fn image_paste_is_documented_in_quick_help_and_full_help() {
        // Standard quick help (wide): the vision qualifier and all three forms.
        let mut wide = model();
        let _ = wide.handle_input(crate::InputAction::Insert("?".to_string()), 76);
        let (buffer, _) = render_test_frame(&wide, 80, 24);
        let text = rect_text(&buffer, buffer.area);
        for needle in ["vision", "bitmap", "file", "path"] {
            assert!(text.contains(needle), "wide quick help missing {needle}");
        }

        // Standard quick help (narrow): same coverage, nothing clipped.
        let mut narrow = model();
        let _ = narrow.handle_input(crate::InputAction::Insert("?".to_string()), 40);
        let (buffer, _) = render_test_frame(&narrow, 48, 24);
        let text = rect_text(&buffer, buffer.area);
        for needle in ["vision", "bitmap", "file", "path"] {
            assert!(text.contains(needle), "narrow quick help missing {needle}");
        }

        // Standard full /help names all three forms and the vision qualifier.
        let mut help = model();
        help.open_help();
        let (buffer, _) = render_test_frame(&help, 90, 30);
        let text = rect_text(&buffer, buffer.area);
        for needle in [
            "vision-capable",
            "bitmap",
            "image file",
            "pasted/dropped image path",
        ] {
            assert!(text.contains(needle), "full help missing {needle}");
        }

        // Pair quick help says unavailable and offers no attach affordance.
        let mut pair_quick = pair_model();
        let _ = pair_quick.handle_input(crate::InputAction::Insert("?".to_string()), 76);
        let (buffer, _) = render_test_frame(&pair_quick, 80, 24);
        let text = rect_text(&buffer, buffer.area);
        assert!(text.contains("unavailable in pair"));
        assert!(!text.contains("paste an image"));
        assert!(!text.contains("Attach an image"));

        // Pair full /help: unavailable, and never an attach claim.
        let mut pair_help = pair_model();
        pair_help.open_help();
        let (buffer, _) = render_test_frame(&pair_help, 90, 30);
        let text = rect_text(&buffer, buffer.area);
        assert!(text.contains("unavailable in pair sessions"));
        assert!(!text.contains("paste an image"));
        assert!(!text.contains("Attach an image"));
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
        assert!(rendered.contains("1. Terminal"));
        assert!(rendered.contains("2. Default ✓"));
        assert!(rendered.contains("❯3. Dim"));
        assert!(rendered.contains("1 - let total = items.len();"));
        assert!(rendered.contains("Enter select · Esc cancel"));
        assert_eq!(app.theme, Theme::Dim);
    }

    #[test]
    fn theme_picker_keeps_all_five_choices_at_minimum_and_is_explicit_for_screen_readers() {
        for screen_reader in [false, true] {
            let mut app = model();
            app.capabilities.screen_reader = screen_reader;
            app.open_theme_picker();
            let mut terminal = Terminal::new(TestBackend::new(30, 10)).expect("terminal");
            let mut hit_map = None;
            terminal
                .draw(|frame| hit_map = Some(render(frame, &app)))
                .expect("draw compact theme picker");

            let rendered = terminal.backend().to_string();
            let hit_map = hit_map.expect("theme hits");
            assert!(rendered.contains("Select a color mode"));
            assert!(rendered.contains("1. Terminal"));
            assert!(rendered.contains("2. Default ✓"));
            assert!(rendered.contains("3. Dim"));
            assert!(rendered.contains("4. High contrast"));
            assert!(rendered.contains("5. Colorblind"));
            assert!(rendered.contains("Enter select · Esc cancel"));
            assert_eq!(hit_map.theme_rows.len(), 5);
            if screen_reader {
                assert!(rendered.contains("Current selection: 2. Default"));
                assert!(!rendered.contains('╭'));
            } else {
                assert!(rendered.contains('╭'));
            }
        }
    }

    #[test]
    fn no_color_theme_picker_names_non_color_diff_cues() {
        let mut app = model();
        app.capabilities.color = ColorSupport::NoColor;
        app.open_theme_picker();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw no-color theme picker");

        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("removed (underlined)"));
        assert!(rendered.contains("added (bold)"));
        assert!(rendered.contains("colors disabled"));
    }

    #[test]
    fn trust_dialog_uses_full_timeline_width_numbered_choices_and_mouse_hits() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = model();
        app.require_workspace_trust("D:\\workspace");
        let mut hit_map = None;
        terminal
            .draw(|frame| {
                hit_map = Some(render(frame, &app));
            })
            .expect("draw trust dialog");
        let rendered = terminal.backend().to_string();
        let hit_map = hit_map.expect("trust hit map");
        let timeline = single_timeline(hit_map.frame.expect("frame layout")).content;
        assert!(rendered.contains("Trust this workspace?"));
        assert!(rendered.contains("D:\\workspace"));
        assert!(rendered.contains("❯ 1. Session only"));
        assert!(rendered.contains("2. Trust and remember"));
        assert!(rendered.contains("3. No - exit"));
        assert!(rendered.contains("↑/↓ to select · enter to confirm · esc to exit"));
        assert_eq!(hit_map.trust_rows.len(), 3);
        assert_eq!(hit_map.trust_rows[0].area.x, timeline.x + 3);
        assert_eq!(hit_map.trust_rows[0].area.width, timeline.width - 6);
        let path_hit = hit_map.trust_path.as_ref().expect("path hit");
        assert_eq!(path_hit.text(), "D:\\workspace");
        assert!(!format!("{path_hit:?}").contains("D:\\workspace"));

        let buffer = terminal.backend().buffer();
        let border_y = hit_map.trust_rows[0].area.y.saturating_sub(6);
        assert_eq!(buffer[(timeline.x, border_y)].symbol(), "╭");
        assert_eq!(
            buffer[(timeline.right().saturating_sub(1), border_y)].symbol(),
            "╮"
        );

        app.start_trust_path_selection(path_hit.text().to_string(), 0);
        app.extend_trust_path_selection(path_hit.text(), 2);
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw selected trust path");
        let selected = terminal.backend().buffer()[(path_hit.area.x, path_hit.area.y)].style();
        assert_eq!(
            selected.bg,
            ThemeResolver::new(Theme::Default, ColorSupport::Color)
                .ui(UiRole::Selection)
                .bg
        );
    }

    #[test]
    fn approval_dialog_uses_original_deny_safe_copy() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = model();

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
    fn trust_dialog_keeps_all_choices_at_the_minimum_supported_frame() {
        for screen_reader in [false, true] {
            let mut app = model();
            app.capabilities.screen_reader = screen_reader;
            app.require_workspace_trust("D:\\a-very-long-workspace-name\\fixture");
            let mut terminal = Terminal::new(TestBackend::new(30, 10)).expect("terminal");
            let mut hits = None;
            terminal
                .draw(|frame| hits = Some(render(frame, &app)))
                .expect("draw compact trust dialog");
            let rendered = terminal.backend().to_string();
            let hits = hits.expect("compact hit map");

            assert_eq!(hits.trust_rows.len(), 3);
            assert!(hits.trust_path.is_some());
            assert!(rendered.contains("1. Session only"));
            assert!(rendered.contains("2. Trust and remember"));
            assert!(rendered.contains("3. No - exit"));
            assert!(!rendered.contains("resize to at least"));
            if screen_reader {
                // The full "Current selection: 1. Session only" line is 34 cols
                // and does not fit a width-30 frame; the three complete labels
                // above already prove the relabel took effect.
                assert!(!rendered.contains('╭'));
            } else {
                assert!(rendered.contains('╭'));
            }
        }
    }

    #[test]
    fn question_dialog_matches_the_numbered_choice_lifecycle_and_exposes_hits() {
        let mut app = model();
        app.apply_runtime(crate::RuntimeUpdate::ToolStarted {
            id: "ask-1".into(),
            name: "ask_user".into(),
            detail: "Do you prefer Red or Blue?".into(),
        });
        app.request_question(
            Some("Preference".to_string()),
            "Do you prefer Red or Blue?",
            [
                crate::QuestionOption {
                    label: "Red".to_string(),
                    description: None,
                },
                crate::QuestionOption {
                    label: "Blue".to_string(),
                    description: Some("cool tone".to_string()),
                },
            ],
            false,
            1,
            1,
        );
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
        let mut hits = None;
        terminal
            .draw(|frame| hits = Some(render(frame, &app)))
            .expect("draw question");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("○ Asking user Do you prefer Red or Blue?"));
        assert!(rendered.contains("Preference"));
        assert!(rendered.contains("❯ 1. Red"));
        assert!(rendered.contains("2. Blue"));
        assert!(rendered.contains("cool tone"));
        assert!(rendered.contains("3. Other (type your answer)"));
        assert!(rendered.contains("↑/↓ to select · enter to confirm · esc …"));
        let hits = hits.expect("hit map");
        assert_eq!(hits.question_rows.len(), 3);
        assert_eq!(hits.question_rows[2].index, 2);
        assert_eq!(hits.question_rows[0].area.width, 40);

        app.capabilities.screen_reader = true;
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw accessible question");
        let accessible = terminal.backend().to_string();
        assert!(accessible.contains("Asking user Do you prefer Red or Blue?"));
        assert!(!accessible.contains("○ Asking user"));
        assert!(accessible.contains("❯ 1. Red"));

        let mut narrow = Terminal::new(TestBackend::new(40, 20)).expect("narrow terminal");
        narrow
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw narrow accessible question");
        let narrow = narrow.backend().to_string();
        assert!(narrow.contains("↑/↓ to select · enter to confirm ·"));
        assert!(narrow.contains("esc to cancel"));

        app.clear_dialog();
        app.apply_runtime(crate::RuntimeUpdate::ToolFinished {
            id: "ask-1".into(),
            name: "ask_user".into(),
            is_error: false,
            cancelled: false,
            output: "tool: ask_user\nstatus: success\noutput:\nUser selected: Red".into(),
            duration_ms: 100,
        });
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw accessible answer");
        let resolved = terminal.backend().to_string();
        assert!(resolved.contains("Asked user Do you prefer Red or Blue?"));
        assert!(resolved.contains("User selected: Red"));
        assert!(!resolved.contains("● Asked user"));
        assert!(!resolved.contains("└ User selected"));
    }

    #[test]
    fn long_question_answer_grows_scrolls_to_the_caret_and_survives_resize() {
        let mut app = model();
        app.request_question(
            Some("Context".to_string()),
            "Explain the constraints",
            Vec::<crate::QuestionOption>::new(),
            false,
            1,
            1,
        );
        let _ = app.handle_question_input(crate::InputAction::Submit);
        let answer = format!("START-OF-ANSWER {} END-OF-ANSWER", "detail ".repeat(150));
        let _ = app.handle_question_input(crate::InputAction::Paste(answer.clone()));

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        let mut hits = None;
        terminal
            .draw(|frame| hits = Some(render(frame, &app)))
            .expect("draw long answer at end");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("END-OF-ANSWER"));
        assert!(!rendered.contains("START-OF-ANSWER"));
        assert!(rendered.contains('█'), "overflow needs a visible scrollbar");
        let hits = hits.expect("hit map");
        assert_eq!(hits.question_rows.len(), 1);
        assert!(hits.question_rows[0].area.height > 1);

        let _ = app.handle_question_input(crate::InputAction::MoveTextStart);
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw long answer at start");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("START-OF-ANSWER"));
        assert!(!rendered.contains("END-OF-ANSWER"));

        let _ = app.handle_question_input(crate::InputAction::MoveTextEnd);
        let mut narrow = Terminal::new(TestBackend::new(40, 20)).expect("narrow terminal");
        narrow
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw resized long answer");
        assert!(narrow.backend().to_string().contains("END-OF-ANSWER"));

        app.capabilities.screen_reader = true;
        let mut accessible = Terminal::new(TestBackend::new(40, 20)).expect("accessible terminal");
        accessible
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw accessible long answer");
        assert!(accessible.backend().to_string().contains("END-OF-ANSWER"));
    }

    #[test]
    fn banner_remains_the_scrollable_conversation_header() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = model();
        let _ = app.append_prompt("first prompt", Some("12:34".to_string()), false);
        let _ = app
            .active_timeline_mut()
            .push(ItemKind::Assistant, "short response");
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
                .active_timeline_mut()
                .push(ItemKind::Assistant, format!("response {number:02}"));
        }
        app.active_timeline_mut().scroll_by(-10_000, 76, 16);
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw held conversation header");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("LocalPilot v0"));
        assert!(rendered.contains("first prompt"));
        let hit_map = hit_map.expect("hit map");
        let layout = hit_map.frame.expect("layout");
        let view = app.active_timeline().view(
            single_timeline(layout).content.width,
            single_timeline(layout).content.height,
        );
        assert_eq!(
            single_hits(&hit_map).scrollbar,
            ScrollbarGeometry::calculate(
                single_timeline(layout).scrollbar,
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
                .active_timeline_mut()
                .push(ItemKind::Assistant, format!("response {number:03}"));
        }
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw overflowing shell");
        let hit_map = hit_map.expect("hit map");
        let timeline = single_hits(&hit_map);
        let thumb = timeline.scrollbar.thumb.expect("scrollbar thumb");
        assert!(thumb.height >= 1);
        assert_eq!(thumb.bottom(), timeline.scrollbar.track.bottom());
        assert!(terminal.backend().to_string().contains('█'));
    }

    #[test]
    fn in_flow_and_pinned_prompts_are_three_row_dark_surfaces() {
        let resolver = ThemeResolver::new(Theme::Default, ColorSupport::Color);

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut app = model();
        let prompt = app
            .active_timeline_mut()
            .push(ItemKind::User, "current prompt")
            .expect("prompt");
        assert!(app.active_timeline_mut().set_pending(prompt, true));
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw in-flow prompt");
        let layout = hit_map.expect("hit map").frame.expect("layout");
        let buffer = terminal.backend().buffer();
        let timeline = single_timeline(layout).content;
        let prompt_y = timeline.y + BANNER_ROWS;
        assert_eq!(buffer[(timeline.x, prompt_y)].symbol(), "▄");
        assert!(buffer_line(buffer, prompt_y + 1).contains("current prompt (pending)"));
        assert_eq!(
            buffer[(timeline.x, prompt_y + 1)].symbol(),
            " ",
            "prompt surfaces must not draw a visible side bar"
        );
        assert_eq!(
            buffer[(timeline.right() - 1, prompt_y + 1)].symbol(),
            " ",
            "prompt surfaces must not draw a visible side bar"
        );
        assert_eq!(
            buffer[(timeline.x + 1, prompt_y + 1)].style().bg,
            resolver.ui(UiRole::Surface).bg
        );
        assert_eq!(buffer[(timeline.x, prompt_y + 2)].symbol(), "▀");

        let mut app = model();
        let _ = app
            .active_timeline_mut()
            .push(ItemKind::User, "pinned prompt");
        for number in 0..80 {
            let _ = app
                .active_timeline_mut()
                .push(ItemKind::Assistant, format!("response {number:03}"));
        }
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw pinned prompt");
        let layout = hit_map.expect("hit map").frame.expect("layout");
        let timeline = single_timeline(layout).content;
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(timeline.x, timeline.y)].symbol(), "▄");
        assert!(buffer_line(buffer, timeline.y + 1).contains("pinned prompt"));
        assert_eq!(buffer[(timeline.x, timeline.y + 2)].symbol(), "▀");
        assert!(buffer_line(buffer, timeline.y + 3).contains("response"));
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
        let _ = app.active_timeline_mut().push(ItemKind::User, "prompt");
        let _ = app
            .active_timeline_mut()
            .push(ItemKind::Assistant, "alpha 界 beta");
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw row hits");
        let hit_map = hit_map.expect("hit map");
        let timeline = single_hits(&hit_map);
        let prompt = timeline
            .rows
            .iter()
            .find(|hit| hit.row.kind == ItemKind::User)
            .expect("prompt hit");
        assert_eq!(prompt.content_x, timeline.timeline.x + 3);

        let response = timeline
            .rows
            .iter()
            .find(|hit| hit.row.kind == ItemKind::Assistant)
            .expect("response hit");
        assert_eq!(response.content_x, timeline.timeline.x + 2);
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
            let mut app = AppModel::new(snapshot_header(), TerminalCapabilities::default());
            app.set_tabs([
                TabId::Session,
                TabId::Plan,
                TabId::Activity,
                TabId::Settings,
            ]);
            app.editor.insert("snapshot draft");
            for number in 0..80 {
                let _ = app.active_timeline_mut().push(
                    ItemKind::Assistant,
                    format!("snapshot response {number:03}"),
                );
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
                buffer[(
                    single_timeline(layout).scrollbar.x,
                    single_timeline(layout).scrollbar.y
                )]
                    .symbol(),
                "│"
            );
            assert!(buffer_line(buffer, layout.status.y).contains("workspace"));
            assert!(buffer_line(buffer, layout.footer.y).contains("Ctrl+C"));
            if (width, height) == (80, 24) {
                // Captured at the last commit before paired render/layout geometry
                // first landed in d7a6115.
                assert_eq!(
                    character_cell_snapshot(buffer),
                    include_str!("fixtures/single_chat_80x24.cells")
                );
            }
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
    fn report_title_is_one_sanitized_display_width_clipped_line() {
        let mut app = model();
        // A title with a newline, a control char, and wide CJK graphemes that
        // exceed a narrow title bar.
        app.open_report(
            "first\u{0007}\nline 日本語テスト padding padding padding".to_string(),
            vec!["body".to_string()],
        );
        let mut terminal = Terminal::new(TestBackend::new(20, 12)).expect("terminal");
        // Renders without panic at narrow width with wide/control characters — a
        // scalar-count clip would overflow the title bar.
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw narrow report");
        let rendered = terminal.backend().to_string();
        assert!(
            !rendered.contains('\u{0007}'),
            "control char stripped from the title"
        );
        assert!(
            !rendered.contains("first\nline"),
            "the newline did not split the title"
        );
        // The title is one line on the top border row (the display-width clip kept
        // the wide-character title inside the bar without panicking above).
        let top = rendered.lines().next().unwrap_or_default();
        assert!(
            top.contains("first") || top.contains("line"),
            "the sanitized title renders on the top row: {top:?}",
        );
    }

    #[test]
    fn reasoning_hidden_in_ordinary_render_mode() {
        // Ordinary (non-screen-reader) mode: the reasoning text itself must vanish.
        let mut app = model();
        let _ = app
            .active_timeline_mut()
            .push(ItemKind::User, "the question");
        app.apply_runtime(crate::RuntimeUpdate::Reasoning("Checking context".into()));
        let _ = app
            .active_timeline_mut()
            .push(ItemKind::Assistant, "the answer");
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw visible");
        assert!(terminal.backend().to_string().contains("Checking context"));
        app.toggle_reasoning();
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw hidden");
        let hidden = terminal.backend().to_string();
        assert!(!hidden.contains("Checking context"));
        assert!(hidden.contains("the answer"));
    }

    #[test]
    fn streamed_segment_glyphs_share_the_first_row_with_prose() {
        let mut app = model();
        app.apply_runtime(crate::RuntimeUpdate::Text("\r\n\nassistant prose".into()));
        app.apply_runtime(crate::RuntimeUpdate::Reasoning("\nreasoning prose".into()));
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");

        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw segments");

        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("● assistant prose"));
        assert!(rendered.contains("◌ reasoning prose"));
    }

    #[test]
    fn reasoning_hidden_is_omitted_from_the_render() {
        let mut app = model();
        app.capabilities.screen_reader = true; // surfaces the "Reasoning: …" label
        let _ = app
            .active_timeline_mut()
            .push(ItemKind::User, "the question");
        app.apply_runtime(crate::RuntimeUpdate::Reasoning("Checking context".into()));
        let _ = app
            .active_timeline_mut()
            .push(ItemKind::Assistant, "the answer");

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw visible");
        let visible = terminal.backend().to_string();
        assert!(visible.contains("Reasoning: Checking context"));
        assert!(visible.contains("the answer"));

        // `/think` hides reasoning from both the screen-reader label and the
        // ordinary rendered text; surrounding items stay.
        app.toggle_reasoning();
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw hidden");
        let hidden = terminal.backend().to_string();
        assert!(!hidden.contains("Reasoning: Checking context"));
        assert!(!hidden.contains("Checking context"));
        assert!(
            hidden.contains("the answer"),
            "surrounding items still render"
        );
    }

    #[test]
    fn screen_reader_projection_linearizes_roles_chrome_dialogs_and_scrollbars() {
        let mut app = model();
        app.capabilities.screen_reader = true;
        app.set_tabs([
            TabId::Session,
            TabId::Plan,
            TabId::Activity,
            TabId::Settings,
        ]);
        let _ = app
            .active_timeline_mut()
            .push(ItemKind::User, "Review this change");
        let _ = app.active_timeline_mut().push(ItemKind::Assistant, "Ready");
        app.apply_runtime(crate::RuntimeUpdate::Reasoning("Checking context".into()));
        app.apply_runtime(crate::RuntimeUpdate::ToolStarted {
            id: "tool-1".into(),
            name: "inspect".into(),
            detail: String::new(),
        });
        app.apply_runtime(crate::RuntimeUpdate::ToolFinished {
            id: "tool-1".into(),
            name: "inspect".into(),
            is_error: false,
            cancelled: false,
            output: String::new(),
            duration_ms: 25,
        });
        let shell = app
            .active_timeline_mut()
            .push(ItemKind::Shell, "ABCDEFGHIJKLMNOPQRSTUVWXYZ")
            .expect("shell row");
        let _ = app
            .active_timeline_mut()
            .set_activity(shell, Some(ActivityState::Success));

        let mut terminal = Terminal::new(TestBackend::new(120, 30)).expect("terminal");
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw screen-reader frame");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Home: current tab: Session; tabs: Plan, Activity, Settings"));
        assert!(rendered.contains("User message"));
        assert!(rendered.contains("Reasoning: Checking context"));
        assert!(rendered.contains("Tool completed: inspect completed · 0 lines · 25 ms"));
        assert!(rendered.contains("Shell completed: "));
        assert!(!rendered.contains("● Ready"));
        assert!(!rendered.contains(">_"));

        let mut wrapped_roles = Terminal::new(TestBackend::new(40, 20)).expect("narrow terminal");
        wrapped_roles
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw wrapped role labels");
        let wrapped_roles = wrapped_roles.backend().to_string();
        assert!(wrapped_roles.contains("Shell completed: "));
        assert!(wrapped_roles.contains("XYZ"));

        for number in 0..80 {
            let _ = app
                .active_timeline_mut()
                .push(ItemKind::Assistant, format!("response {number:03}"));
        }
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw overflowing screen-reader frame");
        let hit_map = hit_map.expect("screen-reader hit map");
        let timeline = single_hits(&hit_map);
        assert!(timeline.scrollbar.total_rows > timeline.scrollbar.viewport_rows);
        assert!(timeline.scrollbar.thumb.is_none());
        assert_eq!(timeline.scrollbar.track, Rect::default());

        app.request_approval("write_file", "src/main.rs", "project write");
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw accessible approval");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Permission required"));
        assert!(rendered.contains("Y allow once"));
        assert!(rendered.contains("N or Esc deny"));
        assert!(!rendered.contains("Current selection"));
        assert!(!rendered.contains("╭"));

        app.require_workspace_trust("D:\\workspace");
        let mut trust_hits = None;
        terminal
            .draw(|frame| trust_hits = Some(render(frame, &app)))
            .expect("draw accessible trust dialog");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Current selection: 1. Session only"));
        assert!(rendered.contains("1. Session only"));
        assert!(rendered.contains("2. Trust and remember"));
        assert!(rendered.contains("3. No - exit"));
        assert!(!rendered.contains("╭"));
        assert_eq!(trust_hits.expect("trust hits").trust_rows.len(), 3);

        let mut narrow = Terminal::new(TestBackend::new(40, 20)).expect("narrow terminal");
        narrow
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw narrow screen-reader frame");
        let rendered = narrow.backend().to_string();
        assert!(rendered.contains("Home: current tab: Session; tabs:"));
        // The numbered choices render in full at this width; the one-line
        // "Current selection:" summary is ellipsis-truncated here, so its full
        // form is asserted at the wide screen-reader frame above instead.
        assert!(rendered.contains("1. Session only"));
        assert!(rendered.contains("2. Trust and remember"));
        assert!(rendered.contains("3. No - exit"));
        assert!(!rendered.contains('›'));
    }

    #[test]
    fn compact_tool_results_render_bounded_connectors_at_supported_widths() {
        let mut app = model();
        app.apply_runtime(crate::RuntimeUpdate::ToolStarted {
            id: "shell".into(),
            name: "run_shell".into(),
            detail: "x".into(),
        });
        app.apply_runtime(crate::RuntimeUpdate::ToolFinished {
            id: "shell".into(),
            name: "run_shell".into(),
            is_error: false,
            cancelled: false,
            output: "one\ntwo\nthree\nfour\nfive".into(),
            duration_ms: 5,
        });

        for (width, height) in [(120, 30), (80, 24), (40, 20)] {
            let (buffer, hits) = render_test_frame(&app, width, height);
            let timeline = single_hits(&hits);
            let text = rect_text(&buffer, timeline.timeline);
            assert!(text.contains("✓ Ran x · 5 lines · 5 ms"));
            assert!(text.contains("│ one"));
            assert!(text.contains("│ two"));
            assert!(text.contains("└ three"));
            assert!(!text.contains("four"));
            assert!(!text.contains("five"));
        }

        app.theme = Theme::Colorblind;
        app.capabilities.color = ColorSupport::NoColor;
        let (buffer, hits) = render_test_frame(&app, 80, 24);
        let text = rect_text(&buffer, single_hits(&hits).timeline);
        assert!(
            text.contains("✓ Ran x"),
            "status remains non-color-readable"
        );

        app.capabilities.screen_reader = true;
        let (buffer, hits) = render_test_frame(&app, 80, 24);
        let text = rect_text(&buffer, single_hits(&hits).timeline);
        assert!(text.contains("Tool completed: Ran x · 5 lines · 5 ms"));
        assert!(text.contains("one"));
        assert!(!text.contains("│ one"));
        assert!(!text.contains("└ three"));
    }

    #[test]
    fn theme_frame_goldens_route_active_tabs_through_the_resolver() {
        for theme_name in Theme::ALL {
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
        let _ = app
            .active_timeline_mut()
            .push(ItemKind::User, "marker marker");
        let _ = app
            .active_timeline_mut()
            .push(ItemKind::Assistant, "new MARKER");
        // `/search <query>` is emitted as an ordinary slash command; the host
        // opens timeline search seeded with the query.
        app.open_timeline_search("marker".to_string());
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
            cancelled: false,
            output: String::new(),
            duration_ms: 25,
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
            let mut header = header();
            header.workspace = "D:\\repos\\LocalX\\LocalPilot".to_string();
            header.branch = Some("terminal-chat-experience".to_string());
            header.workspace_dirty = Some(true);
            header.mode = localpilot_slash::Mode::Agent;
            header.profile = "relaxed".to_string();
            let mut app = AppModel::new(header, TerminalCapabilities::default());
            app.set_active_usage(Some(crate::UsageTotals {
                input_tokens: 12,
                output_tokens: 34,
                cached_input_tokens: 0,
            }));
            app.set_active_context_usage(Some((2_500, 10_000)));
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
    fn set_shared_profile_updates_the_rendered_footer() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut header = header();
        header.mode = localpilot_slash::Mode::Agent;
        header.profile = "relaxed".to_string();
        let mut app = AppModel::new(header, TerminalCapabilities::default());

        // The seeded profile renders in the footer.
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw seeded");
        let layout = hit_map.expect("hit map").frame.expect("layout");
        let footer = buffer_line(terminal.backend().buffer(), layout.footer.bottom() - 1);
        assert!(footer.contains("agent · relaxed → model"));

        // Switching the profile updates the footer truthfully — the projection the
        // full-screen host writes in the same branch that changes the engine.
        app.set_shared_profile("BYPASS");
        let mut hit_map = None;
        terminal
            .draw(|frame| hit_map = Some(render(frame, &app)))
            .expect("draw switched");
        let layout = hit_map.expect("hit map").frame.expect("layout");
        let footer = buffer_line(terminal.backend().buffer(), layout.footer.bottom() - 1);
        assert!(footer.contains("agent · BYPASS → model"));
        assert!(!footer.contains("relaxed"));
    }

    #[test]
    fn collaboration_status_is_additive_and_single_status_stays_exact() {
        let single = model();
        assert_eq!(status_right(&single), "0 tokens");

        let mut pair = AppModel::new_pair(
            snapshot_header(),
            crate::SessionHeader {
                provider: "provider-b".to_string(),
                model: "model-b".to_string(),
                session_id: "session-b".to_string(),
                session_name: None,
            },
            TerminalCapabilities::default(),
        );
        assert!(pair.set_pair_status(crate::PairStatus {
            completed_rounds: 1,
            max_rounds: 3,
            scheduled: Some(PeerPane::B),
            candidate: Some(crate::PairStatusCandidate {
                revision: 2,
                full_digest: "0123456789abcdef".to_string(),
            }),
            agreements: [true, false],
            repairing: None,
            terminal: None,
        }));
        // Running chrome shows the in-flight round, scheduled peer, revision with an
        // eight-character digest, and text-first agreement state.
        assert_eq!(
            status_right(&pair),
            "2/3 rounds · Peer B · r2 01234567 · A agreed · B pending · 0 tokens"
        );

        assert!(pair.set_pair_status(crate::PairStatus {
            completed_rounds: 2,
            max_rounds: 3,
            scheduled: None,
            candidate: None,
            agreements: [true, true],
            repairing: None,
            terminal: Some("Converged".to_string()),
        }));
        assert_eq!(status_right(&pair), "Converged · 2/3 rounds · 0 tokens");
    }

    #[test]
    fn a_result_role_fails_closed_without_a_success_tone() {
        assert_eq!(
            result_role(Some(crate::ResultTone::Success)),
            UiRole::Success
        );
        assert_eq!(
            result_role(Some(crate::ResultTone::Incomplete)),
            UiRole::Warning
        );
        assert_eq!(result_role(Some(crate::ResultTone::Error)), UiRole::Error);
        // A result with no proved tone never wears success chrome.
        assert_eq!(result_role(None), UiRole::Error);
        assert_ne!(result_role(None), UiRole::Success);
    }

    fn pair_status_model() -> AppModel {
        AppModel::new_pair(
            snapshot_header(),
            crate::SessionHeader {
                provider: "provider-b".to_string(),
                model: "model-b".to_string(),
                session_id: "session-b".to_string(),
                session_name: None,
            },
            TerminalCapabilities::default(),
        )
    }

    #[test]
    fn a_result_row_shows_a_distinct_prefix_in_both_accessibility_modes() {
        for (screen_reader, needle) in [(false, "◆"), (true, "Result:")] {
            let mut pair = AppModel::new_pair(
                snapshot_header(),
                crate::SessionHeader {
                    provider: "provider-b".to_string(),
                    model: "model-b".to_string(),
                    session_id: "session-b".to_string(),
                    session_name: None,
                },
                TerminalCapabilities {
                    screen_reader,
                    ..TerminalCapabilities::default()
                },
            );
            assert!(pair.append_result_for(
                PeerPane::A,
                "converged result body".to_string(),
                crate::ResultTone::Success,
            ));
            let backend = TestBackend::new(120, 30);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            terminal
                .draw(|frame| {
                    let _ = render(frame, &pair);
                })
                .expect("draw result row");
            let buffer = terminal.backend().buffer();
            let found = (0..buffer.area.height).any(|y| buffer_line(buffer, y).contains(needle));
            assert!(
                found,
                "screen_reader={screen_reader}: `{needle}` not rendered"
            );
        }
    }

    #[test]
    fn running_chrome_shows_repair_and_reset_with_the_full_digest_retained() {
        let mut pair = pair_status_model();
        // A repair in flight is named; the full digest is retained but abbreviated.
        assert!(pair.set_pair_status(crate::PairStatus {
            completed_rounds: 1,
            max_rounds: 3,
            scheduled: Some(PeerPane::B),
            candidate: Some(crate::PairStatusCandidate {
                revision: 5,
                full_digest: "0123456789abcdef0123".to_string(),
            }),
            agreements: [true, false],
            repairing: Some(PeerPane::B),
            terminal: None,
        }));
        assert_eq!(
            status_right(&pair),
            "2/3 rounds · Peer B · r5 01234567 · A agreed · B pending · Repairing Peer B · 0 tokens"
        );
        assert_eq!(
            pair.pair_status()
                .and_then(|status| status.candidate.as_ref())
                .map(|candidate| candidate.full_digest.as_str()),
            Some("0123456789abcdef0123"),
            "the full digest is retained though the chrome shows only eight characters"
        );

        // A new revision resets both agreements to pending in the visible chrome.
        assert!(pair.set_pair_status(crate::PairStatus {
            completed_rounds: 1,
            max_rounds: 3,
            scheduled: Some(PeerPane::A),
            candidate: Some(crate::PairStatusCandidate {
                revision: 6,
                full_digest: "ff".to_string(),
            }),
            agreements: [false, false],
            repairing: None,
            terminal: None,
        }));
        assert_eq!(
            status_right(&pair),
            "2/3 rounds · Peer A · r6 ff · A pending · B pending · 0 tokens"
        );
    }

    #[test]
    fn a_narrow_status_line_clips_without_overflowing() {
        let mut pair = pair_status_model();
        assert!(pair.set_pair_status(crate::PairStatus {
            completed_rounds: 1,
            max_rounds: 3,
            scheduled: Some(PeerPane::B),
            candidate: Some(crate::PairStatusCandidate {
                revision: 5,
                full_digest: "0123456789abcdef".to_string(),
            }),
            agreements: [true, false],
            repairing: Some(PeerPane::B),
            terminal: None,
        }));
        // The wide status string is longer than a narrow terminal; the existing width
        // policy must clip it to the frame without wrapping or overflowing.
        let width = 48;
        let backend = TestBackend::new(width, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut layout = None;
        terminal
            .draw(|frame| layout = Some(render(frame, &pair).frame.expect("layout")))
            .expect("draw narrow status");
        let layout = layout.expect("frame layout");
        // Confirm the narrow (stacked) status path is exercised.
        assert_eq!(
            layout.status.height, 2,
            "the narrow status stacks into two lines"
        );
        // Precondition: the untruncated status is genuinely wider than the frame, and
        // its full form ends with the token count that truncation should clip.
        let full = status_right(&pair);
        assert!(
            full.chars().count() > usize::from(width),
            "precondition: the untruncated status is wider than the frame"
        );
        assert!(full.ends_with("0 tokens"));
        // `truncate_end` — not mere buffer padding — was applied: the leading round
        // text survives, an ellipsis marks the cut, and the trailing token count is
        // clipped away.
        let line = buffer_line(terminal.backend().buffer(), layout.status.bottom() - 1);
        assert!(
            line.contains("2/3 rounds · Peer B"),
            "leading status text survives truncation: {line:?}"
        );
        assert!(line.contains('…'), "the status was truncated: {line:?}");
        assert!(
            !line.contains("0 tokens"),
            "the trailing status content was clipped: {line:?}"
        );
    }

    #[test]
    fn collaboration_composer_names_the_selected_peer_only() {
        let mut pair = AppModel::new_pair(
            snapshot_header(),
            crate::SessionHeader {
                provider: "provider-b".to_string(),
                model: "model-b".to_string(),
                session_id: "session-b".to_string(),
                session_name: None,
            },
            TerminalCapabilities::default(),
        );

        let composer_line = |app: &AppModel| {
            let backend = TestBackend::new(120, 30);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            let mut hit_map = None;
            terminal
                .draw(|frame| hit_map = Some(render(frame, app)))
                .expect("draw composer target");
            let layout = hit_map.expect("hit map").frame.expect("layout");
            buffer_line(terminal.backend().buffer(), layout.composer.y)
        };

        assert!(composer_line(&pair).contains("Steer Peer A"));
        assert!(pair.select_pair_pane(PeerPane::B));
        assert!(composer_line(&pair).contains("Steer Peer B"));
        assert!(!composer_line(&model()).contains("Steer Peer"));
    }

    #[test]
    fn stashed_footer_indicator_survives_wide_narrow_minimum_and_screen_reader_frames() {
        for (width, height, screen_reader) in [
            (120, 30, false),
            (40, 20, false),
            (30, 10, false),
            (30, 10, true),
        ] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).expect("test terminal");
            let mut app = model();
            app.capabilities.screen_reader = screen_reader;
            app.editor.insert("saved draft");
            let _ = app.handle_input(crate::InputAction::StashOrPop, 80);
            let mut hit_map = None;
            terminal
                .draw(|frame| hit_map = Some(render(frame, &app)))
                .expect("draw stashed footer");
            let layout = hit_map.expect("hit map").frame.expect("layout");
            let footer = (layout.footer.y..layout.footer.bottom())
                .map(|y| buffer_line(terminal.backend().buffer(), y))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                footer.contains("stashed"),
                "missing stash state at {width}x{height}, screen_reader={screen_reader}: {footer:?}"
            );
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

        assert!(footer.contains("Working · 00:00 · 8.9 KiB · Ctrl+C / Esc interrupt"));
        assert!(footer.trim_end().ends_with("model"));
        assert!(!footer.contains("agent · default"));
        assert!(!footer.contains("? help"));
        app.editor.insert("steer next");
        assert!(footer_state(&app).contains("Ctrl+C clear"));
        assert!(footer_state(&app).contains("Ctrl+Q enqueue"));
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
            .active_timeline_mut()
            .push(ItemKind::Assistant, "copy this")
            .expect("timeline item");
        app.active_timeline_mut()
            .start_selection(crate::ContentPoint {
                item_id: item,
                byte: 0,
            });
        app.active_timeline_mut()
            .extend_selection(crate::ContentPoint {
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
                .active_timeline_mut()
                .push(ItemKind::Assistant, format!("response {number:03}"));
        }
        app.active_timeline_mut().scroll_by(-20, 70, 12);
        let crate::ViewportAnchor::Held(anchor) = app.active_timeline().viewport else {
            panic!("timeline must be held");
        };
        let tail = app.active_timeline().items().last().expect("tail").id;
        assert!(app.active_timeline_mut().append_text(tail, " streamed"));
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("draw held stream");
        assert_eq!(
            app.active_timeline().viewport,
            crate::ViewportAnchor::Held(anchor)
        );
        assert!(terminal.backend().to_string().contains("new output"));
    }

    #[test]
    fn footer_describes_each_ctrl_c_rung_for_a_typed_draft() {
        let mut app = model();
        app.editor.insert("draft");
        assert_eq!(footer_state(&app), "idle · Ctrl+C clear draft");

        app.begin_work();
        assert!(footer_state(&app).contains("Ctrl+C clear · Esc interrupt"));
        let _ = app.handle_input(crate::InputAction::CancelOrExit, 76);
        assert!(footer_state(&app).contains("Ctrl+C / Esc interrupt"));
        let _ = app.handle_input(crate::InputAction::CancelOrExit, 76);
        assert_eq!(footer_state(&app), "press Ctrl+C again to exit");
    }

    #[test]
    fn leading_bang_owns_the_shell_mode_footer_until_escape() {
        let mut app = model();
        let _ = app.handle_input(crate::InputAction::Insert("!echo marker".to_string()), 76);
        assert_eq!(app.editor.text(), "echo marker");
        assert_eq!(footer_state(&app), "shell mode · Esc exit shell mode");
        let _ = app.handle_input(crate::InputAction::Escape, 76);
        assert_eq!(app.editor.text(), "echo marker");
        assert_eq!(footer_state(&app), "idle · Ctrl+C clear draft");
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
    fn research_mode_shows_in_footer_and_composer_hint_without_moving_geometry() {
        // The composer renders the Research hint as an empty-editor placeholder in BOTH
        // the normal and screen-reader configurations; it is a placeholder (empty-editor
        // only), so it never shifts composer geometry or the cursor vs an Agent composer.
        // Past the empty-conversation hero splash so the composer renders its editor
        // placeholder rather than the welcome tagline.
        let seed_turn = |app: &mut AppModel| {
            app.begin_work();
            app.apply_runtime(crate::RuntimeUpdate::Text("a prior answer".to_string()));
            app.apply_runtime(crate::RuntimeUpdate::Stopped(crate::StopState::Done));
        };
        for screen_reader in [false, true] {
            let mut research = AppModel::new(
                header(),
                TerminalCapabilities {
                    screen_reader,
                    ..TerminalCapabilities::default()
                },
            );
            seed_turn(&mut research);
            research.set_shared_mode(localpilot_slash::Mode::Research);
            assert_eq!(
                research.composer_hint(),
                Some("Research a topic — local + web per config")
            );
            let (buf_r, hits_r) = render_test_frame(&research, 100, 24);
            assert!(
                rect_text(&buf_r, buf_r.area).contains("Research a topic"),
                "screen_reader={screen_reader}: the empty Research composer renders the hint; buffer:\n{}",
                character_cell_snapshot(&buf_r)
            );

            // Geometry/cursor invariance vs an empty Agent composer (same layout state).
            let mut agent = AppModel::new(
                header(),
                TerminalCapabilities {
                    screen_reader,
                    ..TerminalCapabilities::default()
                },
            );
            seed_turn(&mut agent);
            assert_eq!(agent.composer_hint(), None);
            let (_buf_a, hits_a) = render_test_frame(&agent, 100, 24);
            assert_eq!(
                hits_r.composer, hits_a.composer,
                "screen_reader={screen_reader}: the hint does not move composer geometry"
            );
            assert_eq!(
                hits_r.editor_width, hits_a.editor_width,
                "screen_reader={screen_reader}: the hint does not change editor width"
            );
        }
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

    #[test]
    fn working_chrome_formats_monotonic_elapsed_time_and_motion() {
        assert_eq!(format_elapsed(Duration::ZERO), "00:00");
        assert_eq!(format_elapsed(Duration::from_secs(754)), "12:34");
        assert_eq!(format_elapsed(Duration::from_secs(3_661)), "01:01:01");
        assert_ne!(
            working_glyph(Duration::ZERO),
            working_glyph(Duration::from_millis(200))
        );

        let mut app = model();
        app.begin_work_with_label("Compacting");
        let footer = footer_state(&app);
        assert!(footer.contains("Compacting · 00:00"));
        assert!(footer.contains("Ctrl+C / Esc interrupt"));
    }

    #[test]
    fn localmind_renders_sections_read_data_and_review_controls() {
        let mut app = model();
        app.open_localmind(crate::LocalMindData {
            docs: vec!["guide.md · 3 chunks".to_string()],
            graph: vec!["12 files · 44 symbols".to_string()],
            memory: vec!["memory-1 · workflow".to_string()],
            review: vec![crate::LocalMindReviewRow {
                id: "candidate-1".to_string(),
                state: "Pending".to_string(),
                session_id: "session-1".to_string(),
                summary: "Prefer bounded terminal views".to_string(),
                category: "workflow".to_string(),
                confidence: "92%".to_string(),
                note: None,
                replacement: None,
                seen_count: 1,
                evidence: Some("A large report stayed responsive.".to_string()),
                requires_edit: false,
                promoted: false,
            }],
            skills: vec!["pending · terminal-helper".to_string()],
            audit: vec!["accepted · candidate-1".to_string()],
        });
        let backend = TestBackend::new(90, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let mut hit_map = None;
        terminal
            .draw(|frame| {
                hit_map = Some(render(frame, &app));
            })
            .expect("render docs");
        let rendered = terminal.backend().to_string();
        let hit_map = hit_map.expect("localmind hit map");
        assert_eq!(app.active_tab, TabId::LocalMind);
        assert_eq!(hit_map.tabs.len(), 2);
        assert_eq!(hit_map.tabs[0].tab, TabId::Session);
        assert_eq!(hit_map.tabs[1].tab, TabId::LocalMind);
        assert!(buffer_line(terminal.backend().buffer(), 0).contains("Session"));
        assert!(buffer_line(terminal.backend().buffer(), 0).contains("LocalMind"));
        for label in ["Docs", "Graph", "Memory", "Review", "Skills", "Audit"] {
            assert!(rendered.contains(label), "missing section label {label}");
        }
        assert!(rendered.contains("guide.md · 3 chunks"));
        assert!(rendered.contains("Tab/Shift+Tab section"));

        for _ in 0..3 {
            let _ = app.handle_input(crate::InputAction::AcceptCompletion, 80);
        }
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("render review");
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Prefer bounded terminal views"));
        assert!(rendered.contains("Reviewer: not set"));
        assert!(rendered.contains("a accept · r reject · p promote"));
        assert!(rendered.contains("Evidence: A large report stayed responsive."));
    }

    #[test]
    fn localmind_narrow_render_clips_without_panicking() {
        let mut app = model();
        app.open_localmind(crate::LocalMindData {
            docs: vec!["a/very/long/documentation/path/guide.md · 123 chunks".to_string()],
            ..crate::LocalMindData::default()
        });
        let backend = TestBackend::new(30, 10);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                let _ = render(frame, &app);
            })
            .expect("render narrow localmind");
        assert!(terminal.backend().to_string().contains("LocalMind"));
    }

    #[test]
    fn localmind_text_window_materializes_only_the_requested_viewport() {
        let source = (0..10_000)
            .map(|index| format!("doc-{index}"))
            .collect::<Vec<_>>();
        let app = model();

        let (start, total, visible) = text_takeover_window(&source, usize::MAX, 7, 80, theme(&app));

        assert_eq!(total, 10_000);
        assert_eq!(start, 9_993);
        assert_eq!(visible.len(), 7);
        assert_eq!(visible[0].spans[0].content.as_ref(), "doc-9993");
        assert_eq!(visible[6].spans[0].content.as_ref(), "doc-9999");
    }
}
