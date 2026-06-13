#![allow(clippy::unwrap_used)]

#[path = "../src/key_input.rs"]
mod key_input;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

#[test]
fn alt_enter_variants_insert_newline() {
    for code in [KeyCode::Enter, KeyCode::Char('\r'), KeyCode::Char('\n')] {
        let event = key(code, KeyModifiers::ALT);
        assert!(key_input::is_newline(event, "hello"));
        assert!(!key_input::is_submit(event, "hello"));
    }
}

#[test]
fn shift_enter_inserts_newline_when_reported() {
    let event = key(KeyCode::Enter, KeyModifiers::SHIFT);
    assert!(key_input::is_newline(event, "hello"));
    assert!(!key_input::is_submit(event, "hello"));
}

#[test]
fn plain_enter_submits_non_empty_input() {
    for code in [KeyCode::Enter, KeyCode::Char('\r'), KeyCode::Char('\n')] {
        let event = key(code, KeyModifiers::empty());
        assert!(!key_input::is_newline(event, "hello"));
        assert!(key_input::is_submit(event, "hello"));
    }
}

#[test]
fn plain_enter_submits_slash_commands() {
    let event = key(KeyCode::Enter, KeyModifiers::empty());
    assert!(!key_input::is_newline(event, "/ingest"));
    assert!(key_input::is_submit(event, "/ingest"));
}

#[test]
fn ctrl_j_inserts_newline() {
    let event = key(KeyCode::Char('j'), KeyModifiers::CONTROL);
    assert!(key_input::is_newline(event, "hello"));
    assert!(!key_input::is_submit(event, "hello"));
}

#[test]
fn ctrl_c_cancels() {
    assert!(key_input::is_cancel(key(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL
    )));
    assert!(!key_input::is_cancel(key(
        KeyCode::Char('c'),
        KeyModifiers::empty()
    )));
}

#[test]
fn trailing_backslash_keeps_plain_enter_as_newline() {
    let event = key(KeyCode::Enter, KeyModifiers::empty());
    let input = "hello \\".to_string();
    assert!(key_input::is_newline(event, &input));
    assert!(!key_input::is_submit(event, &input));
}

#[test]
fn only_press_events_are_actions() {
    assert!(key_input::is_key_action(KeyEvent::new_with_kind(
        KeyCode::Left,
        KeyModifiers::empty(),
        KeyEventKind::Press
    )));
    for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
        assert!(!key_input::is_key_action(KeyEvent::new_with_kind(
            KeyCode::Left,
            KeyModifiers::empty(),
            kind
        )));
    }
}

#[test]
fn mouse_tracking_off_writes_all_common_disable_sequences() {
    let mut out = Vec::new();
    key_input::write_mouse_tracking_off(&mut out).unwrap();

    assert_eq!(
        out,
        b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l"
    );
}
