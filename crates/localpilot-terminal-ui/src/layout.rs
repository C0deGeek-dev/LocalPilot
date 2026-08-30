use ratatui::layout::{Constraint, Direction, Layout, Rect};

pub const MINIMUM_WIDTH: u16 = 30;
pub const MINIMUM_HEIGHT: u16 = 10;
/// Minimum frame height when a session label is present above the timeline.
pub const PAIR_MINIMUM_HEIGHT: u16 = MINIMUM_HEIGHT + 1;
/// First width that can hold two supported panes and their divider.
pub const PAIR_WIDE_MINIMUM_WIDTH: u16 = MINIMUM_WIDTH * 2 + 1;
const NARROW_WIDTH: u16 = 60;
const MINIMUM_TIMELINE_HEIGHT: u16 = 3;
const CONTENT_LEFT_INSET: u16 = 2;
const CHROME_RIGHT_INSET: u16 = 2;

pub(crate) const fn tab_height(width: u16, screen_reader: bool) -> u16 {
    if screen_reader && width < NARROW_WIDTH {
        2
    } else {
        1
    }
}
const TIMELINE_SCROLLBAR_GAP: u16 = 1;

/// Geometry for one visible session timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimelinePaneLayout {
    /// Session label reserved by pair presentation; absent in ordinary single chat.
    pub label: Option<Rect>,
    /// Timeline viewport below any session label, including its horizontal gutters.
    pub viewport: Rect,
    /// Text content inside the viewport.
    pub content: Rect,
    /// Scrollbar track inside the viewport's right chrome.
    pub scrollbar: Rect,
}

/// Two timeline panes tiled around one explicit divider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairTimelineLayout {
    pub first: TimelinePaneLayout,
    pub divider: Rect,
    pub second: TimelinePaneLayout,
}

/// The visible timeline geometry for an ordinary chat or a pair presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelineLayout {
    Single(TimelinePaneLayout),
    Pair(PairTimelineLayout),
}

/// The sole source of frame geometry for drawing and input hit-testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLayout {
    pub area: Rect,
    pub tabs: Rect,
    pub timeline: TimelineLayout,
    pub status: Rect,
    pub composer: Rect,
    pub composer_content: Rect,
    pub footer: Rect,
    pub narrow: bool,
    pub stacked: bool,
}

impl FrameLayout {
    #[must_use]
    pub fn calculate(area: Rect, requested_editor_rows: u16) -> Option<Self> {
        Self::calculate_for_mode(area, requested_editor_rows, false)
    }

    #[must_use]
    pub fn calculate_for_mode(
        area: Rect,
        requested_editor_rows: u16,
        screen_reader: bool,
    ) -> Option<Self> {
        let rows = calculate_rows(area, requested_editor_rows, screen_reader, 0)?;
        let timeline = TimelineLayout::Single(timeline_pane(rows.timeline, false));
        Some(Self::from_rows(area, rows, timeline))
    }

    #[must_use]
    pub fn calculate_pair_for_mode(
        area: Rect,
        requested_editor_rows: u16,
        screen_reader: bool,
    ) -> Option<Self> {
        let rows = calculate_rows(area, requested_editor_rows, screen_reader, 1)?;
        let timeline = if area.width < PAIR_WIDE_MINIMUM_WIDTH {
            TimelineLayout::Single(timeline_pane(rows.timeline, true))
        } else {
            TimelineLayout::Pair(pair_timeline(rows.timeline))
        };
        Some(Self::from_rows(area, rows, timeline))
    }

    fn from_rows(area: Rect, rows: FrameRows, timeline: TimelineLayout) -> Self {
        let inset_chrome = |row: Rect| {
            Rect::new(
                row.x.saturating_add(CONTENT_LEFT_INSET),
                row.y,
                row.width
                    .saturating_sub(CONTENT_LEFT_INSET)
                    .saturating_sub(CHROME_RIGHT_INSET),
                row.height,
            )
        };
        let composer = Rect::new(
            rows.composer.x.saturating_add(1),
            rows.composer.y,
            rows.composer.width.saturating_sub(2),
            rows.composer.height,
        );
        let composer_content = Rect::new(
            composer.x.saturating_add(1),
            composer.y.saturating_add(1),
            composer.width.saturating_sub(2),
            composer.height.saturating_sub(2),
        );

        Self {
            area,
            tabs: rows.tabs,
            timeline,
            status: inset_chrome(rows.status),
            composer,
            composer_content,
            footer: inset_chrome(rows.footer),
            narrow: rows.narrow,
            stacked: rows.stacked,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameRows {
    tabs: Rect,
    timeline: Rect,
    status: Rect,
    composer: Rect,
    footer: Rect,
    narrow: bool,
    stacked: bool,
}

fn calculate_rows(
    area: Rect,
    requested_editor_rows: u16,
    screen_reader: bool,
    reserved_timeline_rows: u16,
) -> Option<FrameRows> {
    let minimum_height = MINIMUM_HEIGHT.saturating_add(reserved_timeline_rows);
    if area.width < MINIMUM_WIDTH || area.height < minimum_height {
        return None;
    }

    let narrow = area.width < NARROW_WIDTH;
    let stacked = narrow && area.height >= 14;
    let tabs_height = tab_height(area.width, screen_reader);
    let status_height = if stacked { 2 } else { 1 };
    let footer_height = if stacked { 2 } else { 1 };
    let fixed_without_editor = tabs_height
        .saturating_add(status_height)
        .saturating_add(footer_height)
        .saturating_add(2)
        .saturating_add(MINIMUM_TIMELINE_HEIGHT)
        .saturating_add(reserved_timeline_rows);
    let geometry_cap = area.height.saturating_sub(fixed_without_editor).max(1);
    let responsive_cap = area.height.saturating_div(2).saturating_sub(2).max(1);
    let editor_rows = requested_editor_rows
        .max(1)
        .min(geometry_cap)
        .min(responsive_cap);
    let composer_height = editor_rows.saturating_add(2);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(tabs_height),
            Constraint::Min(MINIMUM_TIMELINE_HEIGHT.saturating_add(reserved_timeline_rows)),
            Constraint::Length(status_height),
            Constraint::Length(composer_height),
            Constraint::Length(footer_height),
        ])
        .split(area);

    Some(FrameRows {
        tabs: rows[0],
        timeline: rows[1],
        status: rows[2],
        composer: rows[3],
        footer: rows[4],
        narrow,
        stacked,
    })
}

fn timeline_pane(area: Rect, labelled: bool) -> TimelinePaneLayout {
    let (label, viewport) = if labelled {
        let label = Rect::new(area.x, area.y, area.width, area.height.min(1));
        let viewport = Rect::new(
            area.x,
            area.y.saturating_add(label.height),
            area.width,
            area.height.saturating_sub(label.height),
        );
        (Some(label), viewport)
    } else {
        (None, area)
    };
    let scrollbar = Rect::new(
        viewport.right().saturating_sub(CHROME_RIGHT_INSET),
        viewport.y,
        1,
        viewport.height,
    );
    let content = Rect::new(
        viewport.x.saturating_add(CONTENT_LEFT_INSET),
        viewport.y,
        viewport
            .width
            .saturating_sub(CONTENT_LEFT_INSET)
            .saturating_sub(CHROME_RIGHT_INSET)
            .saturating_sub(TIMELINE_SCROLLBAR_GAP),
        viewport.height,
    );
    TimelinePaneLayout {
        label,
        viewport,
        content,
        scrollbar,
    }
}

fn pair_timeline(area: Rect) -> PairTimelineLayout {
    let pane_width = area.width.saturating_sub(1);
    let first_width = pane_width / 2;
    let second_width = pane_width.saturating_sub(first_width);
    let first_area = Rect::new(area.x, area.y, first_width, area.height);
    let divider = Rect::new(first_area.right(), area.y, 1, area.height);
    let second_area = Rect::new(divider.right(), area.y, second_width, area.height);
    PairTimelineLayout {
        first: timeline_pane(first_area, true),
        divider,
        second: timeline_pane(second_area, true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single_pane(layout: FrameLayout) -> TimelinePaneLayout {
        let TimelineLayout::Single(pane) = layout.timeline else {
            panic!("expected a single timeline pane")
        };
        pane
    }

    fn timeline_area(layout: FrameLayout) -> Rect {
        match layout.timeline {
            TimelineLayout::Single(pane) => match pane.label {
                Some(label) => Rect::new(
                    label.x,
                    label.y,
                    label.width,
                    label.height.saturating_add(pane.viewport.height),
                ),
                None => pane.viewport,
            },
            TimelineLayout::Pair(pair) => Rect::new(
                pair.first.label.expect("first label").x,
                pair.first.label.expect("first label").y,
                pair.first
                    .viewport
                    .width
                    .saturating_add(pair.divider.width)
                    .saturating_add(pair.second.viewport.width),
                pair.divider.height,
            ),
        }
    }

    fn assert_rect_within(rect: Rect, area: Rect) {
        assert!(rect.x >= area.x);
        assert!(rect.y >= area.y);
        assert!(rect.right() <= area.right());
        assert!(rect.bottom() <= area.bottom());
    }

    fn assert_pane_geometry(pane: TimelinePaneLayout, area: Rect) {
        let label = pane.label.expect("pair pane label");
        assert_eq!(label.height, 1);
        assert_eq!(label.x, pane.viewport.x);
        assert_eq!(label.width, pane.viewport.width);
        assert_eq!(label.bottom(), pane.viewport.y);
        assert!(pane.viewport.height >= MINIMUM_TIMELINE_HEIGHT);
        assert_eq!(pane.content.x, pane.viewport.x + CONTENT_LEFT_INSET);
        assert_eq!(pane.content.y, pane.viewport.y);
        assert_eq!(pane.content.height, pane.viewport.height);
        assert_eq!(pane.scrollbar.width, 1);
        assert_eq!(pane.scrollbar.y, pane.viewport.y);
        assert_eq!(pane.scrollbar.height, pane.viewport.height);
        assert_eq!(
            pane.content.right() + TIMELINE_SCROLLBAR_GAP,
            pane.scrollbar.x
        );
        assert_eq!(pane.scrollbar.x, pane.viewport.right() - CHROME_RIGHT_INSET);
        assert_rect_within(label, area);
        assert_rect_within(pane.viewport, area);
        assert_rect_within(pane.content, pane.viewport);
        assert_rect_within(pane.scrollbar, pane.viewport);
    }

    #[test]
    fn standard_frame_has_target_region_order_and_reserved_gutter() {
        let layout = FrameLayout::calculate(Rect::new(0, 0, 80, 24), 1).expect("layout");
        let timeline = single_pane(layout);
        assert!(timeline.label.is_none());
        assert_eq!(layout.tabs, Rect::new(0, 0, 80, 1));
        assert!(layout.tabs.bottom() <= timeline.viewport.y);
        assert!(timeline.viewport.bottom() <= layout.status.y);
        assert!(layout.status.bottom() <= layout.composer.y);
        assert!(layout.composer.bottom() <= layout.footer.y);
        assert_eq!(timeline.scrollbar.width, 1);
        assert_eq!(timeline.content.x, 2);
        assert_eq!(timeline.scrollbar.x, 78);
        assert_eq!(timeline.content.right() + 1, timeline.scrollbar.x);
        assert_eq!(layout.status.x, timeline.content.x);
        assert_eq!(layout.footer.x, timeline.content.x);
        assert_eq!(layout.status.right(), timeline.scrollbar.x);
        assert_eq!(layout.footer.right(), timeline.scrollbar.x);
        assert_eq!(layout.composer.x, 1);
        assert_eq!(layout.composer_content.x, 2);
        assert_eq!(layout.composer_content.height, 1);
        assert!(!layout.narrow);
    }

    #[test]
    fn ordinary_single_geometry_matches_the_legacy_standard_fixture() {
        assert_eq!(
            FrameLayout::calculate(Rect::new(0, 0, 80, 24), 1),
            Some(FrameLayout {
                area: Rect::new(0, 0, 80, 24),
                tabs: Rect::new(0, 0, 80, 1),
                timeline: TimelineLayout::Single(TimelinePaneLayout {
                    label: None,
                    viewport: Rect::new(0, 1, 80, 18),
                    content: Rect::new(2, 1, 75, 18),
                    scrollbar: Rect::new(78, 1, 1, 18),
                }),
                status: Rect::new(2, 19, 76, 1),
                composer: Rect::new(1, 20, 78, 3),
                composer_content: Rect::new(2, 21, 76, 1),
                footer: Rect::new(2, 23, 76, 1),
                narrow: false,
                stacked: false,
            })
        );
    }

    #[test]
    fn narrow_frame_stacks_status_and_footer_without_starving_timeline() {
        let layout = FrameLayout::calculate(Rect::new(0, 0, 40, 20), 20).expect("layout");
        let timeline = single_pane(layout);
        assert!(layout.narrow);
        assert_eq!(layout.status.height, 2);
        assert_eq!(layout.footer.height, 2);
        assert!(timeline.viewport.height >= MINIMUM_TIMELINE_HEIGHT);
        assert!(layout.stacked);
        assert_eq!(layout.composer_content.height, 8);
    }

    #[test]
    fn narrow_screen_reader_layout_reserves_a_wrapped_tab_sentence() {
        let layout =
            FrameLayout::calculate_for_mode(Rect::new(0, 0, 40, 20), 1, true).expect("layout");
        assert_eq!(layout.tabs.height, 2);
        assert!(single_pane(layout).viewport.height >= MINIMUM_TIMELINE_HEIGHT);
    }

    #[test]
    fn thirty_row_composer_is_bounded_and_keeps_a_timeline() {
        let layout = FrameLayout::calculate(Rect::new(0, 0, 120, 30), u16::MAX).expect("layout");
        assert_eq!(layout.composer_content.height, 13);
        assert!(single_pane(layout).viewport.height >= MINIMUM_TIMELINE_HEIGHT);
    }

    #[test]
    fn undersized_frames_do_not_produce_interactive_geometry() {
        assert!(FrameLayout::calculate(Rect::new(0, 0, 29, 24), 1).is_none());
        assert!(FrameLayout::calculate(Rect::new(0, 0, 80, 9), 1).is_none());
    }

    #[test]
    fn content_and_chrome_gutters_stay_consistent_at_every_supported_width() {
        for width in MINIMUM_WIDTH..=200 {
            let layout =
                FrameLayout::calculate(Rect::new(0, 0, width, 24), 1).expect("supported frame");
            let timeline = single_pane(layout);
            assert!(timeline.label.is_none());
            assert_eq!(timeline.content.x, CONTENT_LEFT_INSET);
            assert_eq!(timeline.content.width, width - 5);
            assert_eq!(timeline.content.right() + 1, timeline.scrollbar.x);
            assert_eq!(layout.status.x, timeline.content.x);
            assert_eq!(layout.footer.x, timeline.content.x);
            assert_eq!(layout.status.right(), timeline.scrollbar.x);
            assert_eq!(layout.footer.right(), timeline.scrollbar.x);
            assert!(timeline.content.width >= 25);
        }
    }

    #[test]
    fn pair_thresholds_preserve_a_label_and_three_timeline_rows() {
        assert!(FrameLayout::calculate_pair_for_mode(Rect::default(), u16::MAX, false).is_none());
        assert!(FrameLayout::calculate_pair_for_mode(Rect::new(0, 0, 29, 24), 1, false).is_none());
        assert!(FrameLayout::calculate_pair_for_mode(Rect::new(0, 0, 61, 10), 1, false).is_none());

        let narrow = FrameLayout::calculate_pair_for_mode(Rect::new(0, 0, 30, 11), 1, false)
            .expect("minimum narrow pair");
        let narrow_pane = single_pane(narrow);
        assert_pane_geometry(narrow_pane, narrow.area);
        assert_eq!(narrow_pane.viewport.height, MINIMUM_TIMELINE_HEIGHT + 1);

        let last_active_only =
            FrameLayout::calculate_pair_for_mode(Rect::new(0, 0, 60, 11), 1, false)
                .expect("last active-only width");
        assert!(matches!(
            last_active_only.timeline,
            TimelineLayout::Single(_)
        ));

        let wide = FrameLayout::calculate_pair_for_mode(Rect::new(0, 0, 61, 11), 1, false)
            .expect("minimum wide pair");
        let TimelineLayout::Pair(wide_timeline) = wide.timeline else {
            panic!("width 61 must produce two panes")
        };
        assert_pane_geometry(wide_timeline.first, wide.area);
        assert_pane_geometry(wide_timeline.second, wide.area);
        assert_eq!(
            wide_timeline.first.viewport.height,
            MINIMUM_TIMELINE_HEIGHT + 1
        );
        assert_eq!(
            wide_timeline.second.viewport.height,
            MINIMUM_TIMELINE_HEIGHT + 1
        );
    }

    #[test]
    fn wide_pair_tiles_non_zero_origin_with_deterministic_odd_spare() {
        let layout =
            FrameLayout::calculate_pair_for_mode(Rect::new(7, 5, 62, 20), 1, false).expect("pair");
        let area = timeline_area(layout);
        let TimelineLayout::Pair(pair) = layout.timeline else {
            panic!("wide pair")
        };
        assert_eq!(pair.first.label.expect("first label").x, area.x);
        assert_eq!(pair.first.viewport.width, 30);
        assert_eq!(pair.divider, Rect::new(area.x + 30, area.y, 1, area.height));
        assert_eq!(pair.second.label.expect("second label").x, area.x + 31);
        assert_eq!(pair.second.viewport.width, 31);
        assert_eq!(pair.second.viewport.right(), area.right());
    }

    #[test]
    fn ordinary_editor_rows_keep_shared_chrome_identical() {
        let area = Rect::new(3, 4, 80, 24);
        let single = FrameLayout::calculate_for_mode(area, 1, false).expect("single");
        let pair = FrameLayout::calculate_pair_for_mode(area, 1, false).expect("pair");
        assert_eq!(single.tabs, pair.tabs);
        assert_eq!(single.status, pair.status);
        assert_eq!(single.composer, pair.composer);
        assert_eq!(single.composer_content, pair.composer_content);
        assert_eq!(single.footer, pair.footer);
    }

    #[test]
    fn pair_editor_cap_reserves_the_label_at_the_first_competing_row() {
        let area = Rect::new(0, 0, 61, 11);
        let single_at_pair_cap = FrameLayout::calculate(area, 2).expect("single at cap");
        let pair_at_pair_cap =
            FrameLayout::calculate_pair_for_mode(area, 2, false).expect("pair at cap");
        assert_eq!(single_at_pair_cap.status, pair_at_pair_cap.status);
        assert_eq!(single_at_pair_cap.composer, pair_at_pair_cap.composer);

        let single_over_pair_cap = FrameLayout::calculate(area, 3).expect("single over cap");
        let pair_over_pair_cap =
            FrameLayout::calculate_pair_for_mode(area, 3, false).expect("pair over cap");
        assert_eq!(
            pair_over_pair_cap.status.y,
            single_over_pair_cap.status.y + 1
        );
        assert_eq!(
            pair_over_pair_cap.composer.y,
            single_over_pair_cap.composer.y + 1
        );
        assert_eq!(
            pair_over_pair_cap.composer.height + 1,
            single_over_pair_cap.composer.height
        );
        let TimelineLayout::Pair(pair_timeline) = pair_over_pair_cap.timeline else {
            panic!("wide pair")
        };
        assert_eq!(pair_timeline.first.viewport.height, MINIMUM_TIMELINE_HEIGHT);
        assert_eq!(
            pair_timeline.second.viewport.height,
            MINIMUM_TIMELINE_HEIGHT
        );
    }

    #[test]
    fn pair_geometry_is_bounded_and_gap_free_across_supported_sizes() {
        for width in MINIMUM_WIDTH..=200 {
            for height in [PAIR_MINIMUM_HEIGHT, 12, 14, 24, 40] {
                for requested_editor_rows in [1, 2, u16::MAX] {
                    for screen_reader in [false, true] {
                        let area = Rect::new(7, 11, width, height);
                        let layout = FrameLayout::calculate_pair_for_mode(
                            area,
                            requested_editor_rows,
                            screen_reader,
                        )
                        .expect("supported pair frame");
                        let timeline = timeline_area(layout);
                        assert_rect_within(layout.tabs, area);
                        assert_rect_within(timeline, area);
                        assert_rect_within(layout.status, area);
                        assert_rect_within(layout.composer, area);
                        assert_rect_within(layout.composer_content, area);
                        assert_rect_within(layout.footer, area);
                        assert!(layout.tabs.bottom() <= timeline.y);
                        assert!(timeline.bottom() <= layout.status.y);
                        assert!(layout.status.bottom() <= layout.composer.y);
                        assert!(layout.composer.bottom() <= layout.footer.y);

                        match layout.timeline {
                            TimelineLayout::Single(pane) => {
                                assert!(width < PAIR_WIDE_MINIMUM_WIDTH);
                                assert_eq!(pane.viewport.width, width);
                                assert_pane_geometry(pane, area);
                            }
                            TimelineLayout::Pair(pair) => {
                                assert!(width >= PAIR_WIDE_MINIMUM_WIDTH);
                                assert!(pair.first.viewport.width >= MINIMUM_WIDTH);
                                assert!(pair.second.viewport.width >= MINIMUM_WIDTH);
                                assert!(pair.first.content.width >= 25);
                                assert!(pair.second.content.width >= 25);
                                assert_pane_geometry(pair.first, area);
                                assert_pane_geometry(pair.second, area);
                                let first_label = pair.first.label.expect("first label");
                                let second_label = pair.second.label.expect("second label");
                                assert_eq!(first_label.right(), pair.divider.x);
                                assert_eq!(pair.divider.right(), second_label.x);
                                assert_eq!(pair.divider.y, timeline.y);
                                assert_eq!(pair.divider.height, timeline.height);
                                assert_eq!(second_label.right(), timeline.right());
                            }
                        }
                    }
                }
            }
        }
    }
}
