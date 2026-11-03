//! The remote status canvas: rows the host fills (each a label and its
//! value), and actions. Like the other remote overlays it is transient and
//! never part of the timeline, a report or an export.

/// One row: what it is about, and what is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusRow {
    pub label: String,
    pub value: String,
}

/// The status canvas.
#[derive(Debug, Clone)]
pub struct StatusView {
    pub title: String,
    pub rows: Vec<StatusRow>,
    pub actions: Vec<String>,
    pub action: usize,
    scroll: std::cell::Cell<usize>,
    page: std::cell::Cell<usize>,
    controls_visible: std::cell::Cell<bool>,
}

impl StatusView {
    #[must_use]
    pub fn new(title: String) -> Self {
        Self {
            title,
            rows: Vec::new(),
            actions: Vec::new(),
            action: 0,
            scroll: std::cell::Cell::new(0),
            page: std::cell::Cell::new(1),
            controls_visible: std::cell::Cell::new(true),
        }
    }

    /// Replace the actions: the selection stays on the same label, or falls
    /// to the last (the safe way out).
    pub fn set_actions(&mut self, actions: Vec<String>) {
        let current = self.actions.get(self.action).cloned();
        self.action = current
            .and_then(|label| actions.iter().position(|action| *action == label))
            .unwrap_or_else(|| actions.len().saturating_sub(1));
        self.actions = actions;
    }

    pub fn move_action(&mut self, forward: bool) {
        let count = self.actions.len().max(1);
        self.action = if forward {
            (self.action + 1) % count
        } else {
            (self.action + count - 1) % count
        };
    }

    #[must_use]
    pub fn selected_action(&self) -> Option<&str> {
        self.actions.get(self.action).map(String::as_str)
    }

    pub fn page_down(&mut self) {
        let step = self.page.get().saturating_sub(1).max(1);
        self.scroll.set(self.scroll.get().saturating_add(step));
    }

    pub fn page_up(&mut self) {
        let step = self.page.get().saturating_sub(1).max(1);
        self.scroll.set(self.scroll.get().saturating_sub(step));
    }

    #[must_use]
    pub fn scroll(&self) -> usize {
        self.scroll.get()
    }

    pub(crate) fn set_shown(&self, start: usize, rows: usize) {
        self.scroll.set(start);
        self.page.set(rows);
    }

    #[must_use]
    pub fn controls_visible(&self) -> bool {
        self.controls_visible.get()
    }

    pub(crate) fn set_controls_visible(&self, visible: bool) {
        self.controls_visible.set(visible);
    }
}
