#![allow(clippy::unwrap_used)]

#[path = "../src/key_input.rs"]
mod key_input;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
    let mut input = "hello \\".to_string();
    assert!(key_input::is_newline(event, &input));
    assert!(!key_input::is_submit(event, &input));
    key_input::insert_newline(&mut input);
    assert_eq!(input, "hello \n");
}
