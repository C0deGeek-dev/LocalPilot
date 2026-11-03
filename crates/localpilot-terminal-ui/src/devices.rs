//! The paired-devices view: a list the host fills (every line is the
//! host's), a selection, actions, and an optional revoke confirmation whose
//! target the host froze when it opened. Like the pairing overlays it is
//! transient: never in the timeline, a report or an export.

/// One device as listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevicesRow {
    /// The device's full id (the host keeps it exact).
    pub id: String,
    /// Its name; shortened first when the row does not fit.
    pub name: String,
    /// What tells it apart and where it stands: short id, scope, grant,
    /// here. Never cut: it wraps under the name when it does not fit.
    pub info: String,
    /// Shown under the row while it is selected.
    pub details: Vec<String>,
}

/// Which part of the view keys move through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevicesFocus {
    Rows,
    Actions,
}

/// A revoke confirmation: what it would revoke (frozen by the host), what
/// happens, and its two actions (Cancel first and selected).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevicesConfirm {
    /// The full id of the device it would revoke.
    pub id: String,
    pub lines: Vec<String>,
    pub actions: Vec<String>,
    pub selected: usize,
}

/// The devices view.
#[derive(Debug, Clone)]
pub struct DevicesView {
    pub title: String,
    /// What the host says above the list (loading, failures, results).
    pub status: Vec<String>,
    pub rows: Vec<DevicesRow>,
    /// The selected row, by index into `rows` (the host keeps it by id).
    pub selected: Option<usize>,
    pub focus: DevicesFocus,
    pub actions: Vec<String>,
    pub action: usize,
    pub confirm: Option<DevicesConfirm>,
    /// The first body row shown (the body scrolls under pinned controls,
    /// and follows the selection).
    scroll: std::cell::Cell<usize>,
    /// Bring the selected row into view at the next draw (set when the
    /// selection moves; manual scrolling clears it, so it is kept).
    follow: std::cell::Cell<bool>,
    /// Body rows shown at the last draw (a page moves one less).
    page: std::cell::Cell<usize>,
    controls_visible: std::cell::Cell<bool>,
}

impl DevicesView {
    #[must_use]
    pub fn new(title: String) -> Self {
        Self {
            title,
            status: Vec::new(),
            rows: Vec::new(),
            selected: None,
            focus: DevicesFocus::Rows,
            actions: Vec::new(),
            action: 0,
            confirm: None,
            scroll: std::cell::Cell::new(0),
            follow: std::cell::Cell::new(true),
            page: std::cell::Cell::new(1),
            controls_visible: std::cell::Cell::new(true),
        }
    }

    /// Replace the actions; the selection stays on the same label, or falls
    /// to the last (always the safe way out).
    pub fn set_actions(&mut self, actions: Vec<String>) {
        let current = self.actions.get(self.action).cloned();
        self.action = current
            .and_then(|label| actions.iter().position(|action| *action == label))
            .unwrap_or_else(|| actions.len().saturating_sub(1));
        self.actions = actions;
    }

    #[must_use]
    pub fn selected_action(&self) -> Option<&str> {
        self.actions.get(self.action).map(String::as_str)
    }

    pub fn move_action(&mut self, forward: bool) {
        let count = self.actions.len().max(1);
        self.action = if forward {
            (self.action + 1) % count
        } else {
            (self.action + count - 1) % count
        };
    }

    /// Move the row selection (when there are rows).
    pub fn move_row(&mut self, forward: bool) {
        self.follow.set(true);
        if self.rows.is_empty() {
            self.selected = None;
            return;
        }
        let last = self.rows.len() - 1;
        self.selected = Some(match self.selected {
            None => 0,
            Some(at) if forward => (at + 1).min(last),
            Some(at) => at.saturating_sub(1),
        });
    }

    /// The selected row's full id.
    #[must_use]
    pub fn selected_id(&self) -> Option<&str> {
        self.selected
            .and_then(|at| self.rows.get(at))
            .map(|row| row.id.as_str())
    }

    /// Scroll the body by hand: it stays where it is put (the selection is
    /// not brought back into view until it moves).
    pub fn scroll_by(&mut self, rows: isize) {
        self.follow.set(false);
        self.scroll
            .set(self.scroll.get().saturating_add_signed(rows));
    }

    /// One page down: the body height shown, less one row of overlap, so no
    /// row is ever skipped.
    pub fn page_down(&mut self) {
        let step = self.page_step();
        self.scroll_by(step);
    }

    pub fn page_up(&mut self) {
        let step = self.page_step();
        self.scroll_by(-step);
    }

    fn page_step(&self) -> isize {
        isize::try_from(self.page.get().saturating_sub(1).max(1)).unwrap_or(1)
    }

    pub(crate) fn set_page(&self, rows: usize) {
        self.page.set(rows);
    }

    /// Whether the next draw brings the selection into view (once).
    pub(crate) fn take_follow(&self) -> bool {
        self.follow.replace(false)
    }

    /// The first body row shown when last drawn (or asked for since).
    #[must_use]
    pub fn scroll(&self) -> usize {
        self.scroll.get()
    }

    pub(crate) fn set_scroll(&self, scroll: usize) {
        self.scroll.set(scroll);
    }

    /// Whether the controls were on screen when last drawn.
    #[must_use]
    pub fn controls_visible(&self) -> bool {
        self.controls_visible.get()
    }

    pub(crate) fn set_controls_visible(&self, visible: bool) {
        self.controls_visible.set(visible);
    }
}

impl DevicesConfirm {
    pub fn move_action(&mut self, forward: bool) {
        let count = self.actions.len().max(1);
        self.selected = if forward {
            (self.selected + 1) % count
        } else {
            (self.selected + count - 1) % count
        };
    }

    #[must_use]
    pub fn selected_action(&self) -> Option<&str> {
        self.actions.get(self.selected).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_move_within_bounds_and_actions_fall_to_the_safe_one() {
        let mut view = DevicesView::new("Devices".into());
        view.move_row(true);
        assert_eq!(view.selected, None);
        view.rows = vec![
            DevicesRow {
                id: "a".into(),
                name: "A".into(),
                info: String::new(),
                details: Vec::new(),
            },
            DevicesRow {
                id: "b".into(),
                name: "B".into(),
                info: String::new(),
                details: Vec::new(),
            },
        ];
        view.move_row(true);
        assert_eq!(view.selected_id(), Some("a"));
        view.move_row(true);
        view.move_row(true);
        assert_eq!(view.selected_id(), Some("b"));
        view.set_actions(vec![
            "Revoke device...".into(),
            "Refresh".into(),
            "Close".into(),
        ]);
        assert_eq!(view.selected_action(), Some("Close"));
        view.move_action(false);
        view.move_action(false);
        assert_eq!(view.selected_action(), Some("Revoke device..."));
        view.set_actions(vec!["Pair device".into(), "Close".into()]);
        assert_eq!(view.selected_action(), Some("Close"));
    }
}
