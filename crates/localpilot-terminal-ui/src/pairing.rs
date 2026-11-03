//! Pairing a device from the terminal: the setup form and the view that
//! shows a pairing code. Both are transient overlays the host drives; the
//! code itself lives only in the view (never in the timeline, a report, a
//! copy of the transcript or an export) and is wiped when it goes.

use crate::app::sanitize_inline;

/// The longest device name the server keeps.
pub const DEVICE_NAME_MAX: usize = 64;

/// A field of the setup form, in focus order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingField {
    Name,
    Scope,
    Reach,
    Create,
    Cancel,
}

const FIELDS: [PairingField; 5] = [
    PairingField::Name,
    PairingField::Scope,
    PairingField::Reach,
    PairingField::Create,
    PairingField::Cancel,
];

/// The setup form: what the new device is called and what it may do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingSetup {
    pub name: String,
    /// Control (prompts, cancel, permission answers) rather than observe.
    pub control: bool,
    /// Only this workspace, rather than every served workspace.
    pub this_workspace_only: bool,
    pub focus: PairingField,
    /// Where typing goes in the name, in characters.
    pub cursor: usize,
    /// Why Create did nothing, if it did nothing.
    pub problem: Option<String>,
    /// Whether the last frame showed the form's controls.
    controls_visible: std::cell::Cell<bool>,
}

impl PairingSetup {
    #[must_use]
    pub fn new(control: bool) -> Self {
        Self {
            name: "phone".to_string(),
            control,
            this_workspace_only: false,
            focus: PairingField::Name,
            cursor: "phone".chars().count(),
            problem: None,
            controls_visible: std::cell::Cell::new(true),
        }
    }

    /// Whether the controls were on screen when last drawn.
    #[must_use]
    pub fn controls_visible(&self) -> bool {
        self.controls_visible.get()
    }

    pub(crate) fn set_controls_visible(&self, visible: bool) {
        self.controls_visible.set(visible);
    }

    /// The same form with `name` (the cursor at its end).
    #[must_use]
    pub fn named(mut self, name: &str) -> Self {
        self.name = name.chars().take(DEVICE_NAME_MAX).collect();
        self.cursor = self.name.chars().count();
        self
    }

    fn byte_at(&self, cursor: usize) -> usize {
        self.name
            .char_indices()
            .nth(cursor)
            .map_or(self.name.len(), |(at, _)| at)
    }

    /// Move the name's cursor one character (when the name has focus).
    pub fn move_cursor(&mut self, forward: bool) -> bool {
        if self.focus != PairingField::Name {
            return false;
        }
        let count = self.name.chars().count();
        self.cursor = if forward {
            (self.cursor + 1).min(count)
        } else {
            self.cursor.saturating_sub(1)
        };
        true
    }

    /// Put the name's cursor at its start or end.
    pub fn cursor_to(&mut self, end: bool) -> bool {
        if self.focus != PairingField::Name {
            return false;
        }
        self.cursor = if end { self.name.chars().count() } else { 0 };
        true
    }

    /// Delete the character after the cursor.
    pub fn delete(&mut self) -> bool {
        if self.focus != PairingField::Name || self.cursor >= self.name.chars().count() {
            return false;
        }
        let at = self.byte_at(self.cursor);
        self.name.remove(at);
        self.problem = None;
        true
    }

    /// Move focus to the next (or previous) field.
    pub fn move_focus(&mut self, forward: bool) {
        let at = FIELDS
            .iter()
            .position(|field| *field == self.focus)
            .unwrap_or(0);
        let count = FIELDS.len();
        let next = if forward {
            (at + 1) % count
        } else {
            (at + count - 1) % count
        };
        self.focus = FIELDS[next];
    }

    /// Flip the focused choice (Scope or Reach). Returns whether it changed.
    pub fn toggle(&mut self) -> bool {
        match self.focus {
            PairingField::Scope => self.control = !self.control,
            PairingField::Reach => self.this_workspace_only = !self.this_workspace_only,
            _ => return false,
        }
        true
    }

    /// Type into the name (when it has focus): printable characters only, up
    /// to the server's limit.
    pub fn type_char(&mut self, character: char) -> bool {
        if self.focus != PairingField::Name
            || character.is_control()
            || self.name.chars().count() >= DEVICE_NAME_MAX
        {
            return false;
        }
        let at = self.byte_at(self.cursor);
        self.name.insert(at, character);
        self.cursor += 1;
        self.problem = None;
        true
    }

    /// Delete the character before the cursor.
    pub fn backspace(&mut self) -> bool {
        if self.focus != PairingField::Name || self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        let at = self.byte_at(self.cursor);
        self.name.remove(at);
        self.problem = None;
        true
    }

    /// The name to pair with, or why there is none.
    pub fn device_name(&self) -> Result<String, String> {
        let name = sanitize_inline(self.name.trim());
        if name.trim().is_empty() {
            Err("Give the device a name.".to_string())
        } else {
            Ok(name.trim().chars().take(DEVICE_NAME_MAX).collect())
        }
    }
}

/// A pairing code, as shown: the exact link and its QR rows (if one could be
/// drawn). Wiped when dropped; never printed by `Debug`.
pub struct PairingSecret {
    link: String,
    qr: Option<Vec<String>>,
}

impl PairingSecret {
    #[must_use]
    pub fn new(link: String, qr: Option<Vec<String>>) -> Self {
        Self { link, qr }
    }

    #[must_use]
    pub fn link(&self) -> &str {
        &self.link
    }

    #[must_use]
    pub fn qr(&self) -> Option<&[String]> {
        self.qr.as_deref()
    }
}

fn wipe(text: &mut String) {
    let mut bytes = std::mem::take(text).into_bytes();
    bytes.fill(0);
}

impl Drop for PairingSecret {
    fn drop(&mut self) {
        wipe(&mut self.link);
        for row in self.qr.iter_mut().flatten() {
            wipe(row);
        }
    }
}

/// A copy is wiped when it is dropped, like the original.
impl Clone for PairingSecret {
    fn clone(&self) -> Self {
        Self {
            link: self.link.clone(),
            qr: self.qr.clone(),
        }
    }
}

impl std::fmt::Debug for PairingSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PairingSecret(<redacted>)")
    }
}

/// The pairing view: what is happening, the code while it is live, and the
/// actions that make sense now.
#[derive(Debug, Clone)]
pub struct PairingView {
    pub title: String,
    pub lines: Vec<String>,
    secret: Option<PairingSecret>,
    /// Show the link rather than the QR code.
    pub show_link: bool,
    /// The first row of the link shown (it scrolls when it does not fit).
    pub scroll: usize,
    pub actions: Vec<String>,
    pub selected: usize,
    /// Whether the last frame showed the actions (a window too small for
    /// them shows only how to leave, and the host then takes only Esc).
    controls_visible: std::cell::Cell<bool>,
}

impl PairingView {
    #[must_use]
    pub fn new(title: String, lines: Vec<String>, actions: Vec<String>, show_link: bool) -> Self {
        Self {
            title,
            lines,
            secret: None,
            show_link,
            scroll: 0,
            actions,
            selected: 0,
            controls_visible: std::cell::Cell::new(true),
        }
    }

    /// Replace what the view says and offers. The selection stays on the same
    /// action when it is still offered; otherwise it moves to the last one,
    /// which is always the safe way out (Close, Hide or Cancel), so a key
    /// pressed as the view changes never does something new.
    pub fn update(&mut self, title: String, lines: Vec<String>, actions: Vec<String>) {
        let current = self.actions.get(self.selected).cloned();
        self.title = title;
        self.lines = lines;
        self.selected = current
            .and_then(|label| actions.iter().position(|action| *action == label))
            .unwrap_or_else(|| actions.len().saturating_sub(1));
        self.actions = actions;
    }

    /// Whether the actions were on screen when last drawn.
    #[must_use]
    pub fn controls_visible(&self) -> bool {
        self.controls_visible.get()
    }

    pub(crate) fn set_controls_visible(&self, visible: bool) {
        self.controls_visible.set(visible);
    }

    pub fn show(&mut self, secret: PairingSecret) {
        self.secret = Some(secret);
        self.scroll = 0;
    }

    /// Wipe the code (the view itself may stay, saying why).
    pub fn wipe_secret(&mut self) {
        self.secret = None;
    }

    #[must_use]
    pub fn secret(&self) -> Option<&PairingSecret> {
        self.secret.as_ref()
    }

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

    /// Scroll the view's body by `rows` (negative is up); clamped when drawn.
    pub fn scroll_link(&mut self, rows: isize) {
        self.scroll = self.scroll.saturating_add_signed(rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_form_starts_on_the_name_and_cycles_through_every_field() {
        let mut setup = PairingSetup::new(true);
        assert_eq!(setup.focus, PairingField::Name);
        assert_eq!(setup.name, "phone");
        for expected in [
            PairingField::Scope,
            PairingField::Reach,
            PairingField::Create,
            PairingField::Cancel,
            PairingField::Name,
        ] {
            setup.move_focus(true);
            assert_eq!(setup.focus, expected);
        }
        setup.move_focus(false);
        assert_eq!(setup.focus, PairingField::Cancel);
    }

    #[test]
    fn only_the_focused_choice_toggles_and_only_the_name_takes_text() {
        let mut setup = PairingSetup::new(true);
        assert!(!setup.toggle());
        assert!(setup.type_char('2'));
        assert!(!setup.type_char('\u{1b}'), "no control characters");
        setup.move_focus(true);
        assert!(!setup.type_char('x'));
        assert!(setup.toggle());
        assert!(!setup.control);
        setup.move_focus(true);
        assert!(setup.toggle());
        assert!(setup.this_workspace_only);
        assert_eq!(setup.device_name().unwrap(), "phone2");
    }

    #[test]
    fn a_name_is_required_and_bounded() {
        let mut setup = PairingSetup::new(true);
        while setup.backspace() {}
        assert!(setup.device_name().is_err());
        for _ in 0..100 {
            setup.type_char('a');
        }
        assert_eq!(setup.name.chars().count(), DEVICE_NAME_MAX);
    }

    #[test]
    fn a_secret_never_prints_and_the_selection_follows_its_action() {
        let mut view = PairingView::new(
            "Pair a device".into(),
            Vec::new(),
            vec!["Show link".into(), "Copy link".into(), "Close".into()],
            false,
        );
        view.show(PairingSecret::new(
            "https://pc.example/#pair=xyzzy".into(),
            Some(vec!["row".into()]),
        ));
        assert!(!format!("{view:?}").contains("xyzzy"));
        view.move_action(true);
        view.update(
            "Pair a device".into(),
            Vec::new(),
            vec!["Show QR code".into(), "Copy link".into(), "Close".into()],
        );
        assert_eq!(view.selected_action(), Some("Copy link"));
        // A result that no longer offers it lands on the safe last action,
        // never on a new one placed first.
        view.update(
            "Device paired".into(),
            Vec::new(),
            vec!["Share this session".into(), "Close".into()],
        );
        assert_eq!(view.selected_action(), Some("Close"));
        view.wipe_secret();
        assert!(view.secret().is_none());
    }

    #[test]
    fn the_name_edits_at_its_cursor() {
        let mut setup = PairingSetup::new(true);
        assert_eq!(setup.cursor, 5);
        setup.move_cursor(false);
        setup.move_cursor(false);
        assert!(setup.type_char('X'));
        assert_eq!(setup.name, "phoXne");
        assert!(setup.backspace());
        assert!(setup.delete());
        assert_eq!(setup.name, "phoe");
        setup.cursor_to(false);
        setup.type_char('é');
        assert_eq!(setup.name, "éphoe");
        setup.cursor_to(true);
        assert!(!setup.delete(), "nothing after the end");
        setup.move_focus(true);
        assert!(!setup.move_cursor(false), "only while the name has focus");
    }
}
