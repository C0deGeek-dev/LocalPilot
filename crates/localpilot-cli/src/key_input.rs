use std::io::{self, Write};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

const MOUSE_TRACKING_OFF: &[u8] = b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l";
const UNBRACKETED_PASTE_WINDOW: Duration = Duration::from_millis(35);

pub(crate) fn write_mouse_tracking_off(out: &mut impl Write) -> io::Result<()> {
    out.write_all(MOUSE_TRACKING_OFF)?;
    out.flush()
}

#[cfg_attr(test, allow(dead_code))]
pub(crate) fn mouse_capture_enabled() -> bool {
    matches!(
        std::env::var("LOCALPILOT_ENABLE_MOUSE_CAPTURE")
            .ok()
            .as_deref(),
        Some("1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON")
    )
}

pub(crate) fn is_key_action(key: KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnbracketedPasteAction {
    None,
    InsertNewline,
    Suppress,
}

#[derive(Debug, Default)]
pub(crate) struct UnbracketedPaste {
    active_until: Option<Instant>,
    suppress_lf_until: Option<Instant>,
}

impl UnbracketedPaste {
    pub(crate) fn observe_key(
        &mut self,
        key: KeyEvent,
        buffered_after: bool,
        now: Instant,
    ) -> UnbracketedPasteAction {
        let Some(c) = unmodified_char(key) else {
            self.active_until = None;
            self.suppress_lf_until = None;
            return UnbracketedPasteAction::None;
        };

        match c {
            '\n' if self.suppress_lf_until.is_some_and(|until| now <= until) => {
                self.suppress_lf_until = None;
                UnbracketedPasteAction::Suppress
            }
            '\n' | '\r'
                if buffered_after || self.active_until.is_some_and(|until| now <= until) =>
            {
                self.active_until = Some(now + UNBRACKETED_PASTE_WINDOW);
                self.suppress_lf_until = (c == '\r').then_some(now + UNBRACKETED_PASTE_WINDOW);
                UnbracketedPasteAction::InsertNewline
            }
            '\n' | '\r' => {
                self.active_until = None;
                self.suppress_lf_until = None;
                UnbracketedPasteAction::None
            }
            _ if buffered_after => {
                self.active_until = Some(now + UNBRACKETED_PASTE_WINDOW);
                self.suppress_lf_until = None;
                UnbracketedPasteAction::None
            }
            _ => {
                self.suppress_lf_until = None;
                UnbracketedPasteAction::None
            }
        }
    }
}

pub(crate) fn may_be_unbracketed_paste_key(key: KeyEvent) -> bool {
    unmodified_char(key).is_some()
}

pub(crate) fn is_unbracketed_paste_newline_key(key: KeyEvent) -> bool {
    matches!(unmodified_char(key), Some('\n' | '\r'))
}

pub(crate) fn is_cancel(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL))
}

pub(crate) fn is_submit(key: KeyEvent, input: &str) -> bool {
    is_plain_enter(key)
        && key.modifiers.is_empty()
        && !input.trim().is_empty()
        && !ends_with_continuation(input)
}

/// A keypress that inserts a newline rather than submitting. Several paths are
/// accepted because terminals disagree about how modified Enter is reported:
/// enhanced-key Enter with modifiers, Alt-modified carriage return/newline
/// characters, Ctrl+J, and a trailing backslash before a plain Enter.
pub(crate) fn is_newline(key: KeyEvent, input: &str) -> bool {
    match key.code {
        KeyCode::Char('\n' | '\r') if key.modifiers.contains(KeyModifiers::ALT) => true,
        KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => true,
        KeyCode::Enter
            if key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT) =>
        {
            true
        }
        KeyCode::Enter => ends_with_continuation(input),
        _ => false,
    }
}

fn ends_with_continuation(input: &str) -> bool {
    input.trim_end_matches(' ').ends_with('\\')
}

fn is_plain_enter(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Enter | KeyCode::Char('\n' | '\r'))
}

fn unmodified_char(key: KeyEvent) -> Option<char> {
    if key.modifiers.is_empty() {
        if let KeyCode::Char(c) = key.code {
            return Some(c);
        }
    }
    None
}
