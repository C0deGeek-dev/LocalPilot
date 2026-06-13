use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

pub(crate) fn is_key_action(key: KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
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
