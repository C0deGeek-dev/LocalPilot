use ratatui::layout::{Constraint, Direction, Layout, Rect};

pub const MINIMUM_WIDTH: u16 = 30;
pub const MINIMUM_HEIGHT: u16 = 10;
const NARROW_WIDTH: u16 = 60;
const MINIMUM_TIMELINE_HEIGHT: u16 = 3;

/// The sole source of frame geometry for drawing and input hit-testing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLayout {
    pub area: Rect,
    pub tabs: Rect,
    pub timeline: Rect,
    pub timeline_content: Rect,
    pub scrollbar: Rect,
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
        if area.width < MINIMUM_WIDTH || area.height < MINIMUM_HEIGHT {
            return None;
        }

        let narrow = area.width < NARROW_WIDTH;
        let stacked = narrow && area.height >= 14;
        let status_height = if stacked { 2 } else { 1 };
        let footer_height = if stacked { 2 } else { 1 };
        let fixed_without_editor = 1u16
            .saturating_add(status_height)
            .saturating_add(footer_height)
            .saturating_add(2)
            .saturating_add(MINIMUM_TIMELINE_HEIGHT);
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
                Constraint::Length(1),
                Constraint::Min(MINIMUM_TIMELINE_HEIGHT),
                Constraint::Length(status_height),
                Constraint::Length(composer_height),
                Constraint::Length(footer_height),
            ])
            .split(area);

        let timeline = rows[1];
        let timeline_content = Rect::new(
            timeline.x.saturating_add(1),
            timeline.y,
            timeline.width.saturating_sub(2),
            timeline.height,
        );
        let scrollbar = Rect::new(
            timeline.right().saturating_sub(1),
            timeline.y,
            1,
            timeline.height,
        );
        let composer = rows[3];
        let composer_content = Rect::new(
            composer.x.saturating_add(1),
            composer.y.saturating_add(1),
            composer.width.saturating_sub(2),
            composer.height.saturating_sub(2),
        );

        Some(Self {
            area,
            tabs: rows[0],
            timeline,
            timeline_content,
            scrollbar,
            status: rows[2],
            composer,
            composer_content,
            footer: rows[4],
            narrow,
            stacked,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_frame_has_target_region_order_and_reserved_gutter() {
        let layout = FrameLayout::calculate(Rect::new(0, 0, 80, 24), 1).expect("layout");
        assert_eq!(layout.tabs, Rect::new(0, 0, 80, 1));
        assert!(layout.tabs.bottom() <= layout.timeline.y);
        assert!(layout.timeline.bottom() <= layout.status.y);
        assert!(layout.status.bottom() <= layout.composer.y);
        assert!(layout.composer.bottom() <= layout.footer.y);
        assert_eq!(layout.scrollbar.width, 1);
        assert_eq!(layout.timeline_content.right(), layout.scrollbar.x);
        assert_eq!(layout.composer_content.height, 1);
        assert!(!layout.narrow);
    }

    #[test]
    fn narrow_frame_stacks_status_and_footer_without_starving_timeline() {
        let layout = FrameLayout::calculate(Rect::new(0, 0, 40, 20), 20).expect("layout");
        assert!(layout.narrow);
        assert_eq!(layout.status.height, 2);
        assert_eq!(layout.footer.height, 2);
        assert!(layout.timeline.height >= MINIMUM_TIMELINE_HEIGHT);
        assert!(layout.stacked);
        assert_eq!(layout.composer_content.height, 8);
    }

    #[test]
    fn thirty_row_composer_is_bounded_and_keeps_a_timeline() {
        let layout = FrameLayout::calculate(Rect::new(0, 0, 120, 30), u16::MAX).expect("layout");
        assert_eq!(layout.composer_content.height, 13);
        assert!(layout.timeline.height >= MINIMUM_TIMELINE_HEIGHT);
    }

    #[test]
    fn undersized_frames_do_not_produce_interactive_geometry() {
        assert!(FrameLayout::calculate(Rect::new(0, 0, 29, 24), 1).is_none());
        assert!(FrameLayout::calculate(Rect::new(0, 0, 80, 9), 1).is_none());
    }
}
