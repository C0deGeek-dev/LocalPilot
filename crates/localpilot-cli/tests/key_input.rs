#![allow(clippy::unwrap_used)]

#[path = "../src/key_input.rs"]
mod key_input;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::time::{Duration, Instant};

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

#[test]
fn unbracketed_paste_newline_is_inserted_when_text_is_buffered() {
    let now = Instant::now();
    let mut paste = key_input::UnbracketedPaste::default();

    assert_eq!(
        paste.observe_key(key(KeyCode::Char('a'), KeyModifiers::empty()), true, now),
        key_input::UnbracketedPasteAction::None
    );
    assert_eq!(
        paste.observe_key(
            key(KeyCode::Char('\r'), KeyModifiers::empty()),
            true,
            now + Duration::from_millis(1)
        ),
        key_input::UnbracketedPasteAction::InsertNewline
    );
}

#[test]
fn unbracketed_paste_state_keeps_final_trailing_newline_from_submitting() {
    let now = Instant::now();
    let mut paste = key_input::UnbracketedPaste::default();

    paste.observe_key(key(KeyCode::Char('a'), KeyModifiers::empty()), true, now);
    paste.observe_key(
        key(KeyCode::Char('\n'), KeyModifiers::empty()),
        true,
        now + Duration::from_millis(1),
    );
    paste.observe_key(
        key(KeyCode::Char('b'), KeyModifiers::empty()),
        false,
        now + Duration::from_millis(2),
    );

    assert_eq!(
        paste.observe_key(
            key(KeyCode::Char('\n'), KeyModifiers::empty()),
            false,
            now + Duration::from_millis(3)
        ),
        key_input::UnbracketedPasteAction::InsertNewline
    );
}

#[test]
fn crlf_unbracketed_paste_suppresses_the_lf_half() {
    let now = Instant::now();
    let mut paste = key_input::UnbracketedPaste::default();

    paste.observe_key(key(KeyCode::Char('a'), KeyModifiers::empty()), true, now);
    assert_eq!(
        paste.observe_key(
            key(KeyCode::Char('\r'), KeyModifiers::empty()),
            true,
            now + Duration::from_millis(1)
        ),
        key_input::UnbracketedPasteAction::InsertNewline
    );
    assert_eq!(
        paste.observe_key(
            key(KeyCode::Char('\n'), KeyModifiers::empty()),
            true,
            now + Duration::from_millis(2)
        ),
        key_input::UnbracketedPasteAction::Suppress
    );
}

#[test]
fn standalone_char_newline_keeps_existing_submit_path() {
    let now = Instant::now();
    let mut paste = key_input::UnbracketedPaste::default();

    assert_eq!(
        paste.observe_key(key(KeyCode::Char('\n'), KeyModifiers::empty()), false, now),
        key_input::UnbracketedPasteAction::None
    );
}

#[test]
fn only_unmodified_chars_are_unbracketed_paste_candidates() {
    assert!(key_input::may_be_unbracketed_paste_key(key(
        KeyCode::Char('a'),
        KeyModifiers::empty()
    )));
    assert!(key_input::is_unbracketed_paste_newline_key(key(
        KeyCode::Char('\n'),
        KeyModifiers::empty()
    )));
    assert!(!key_input::may_be_unbracketed_paste_key(key(
        KeyCode::Enter,
        KeyModifiers::empty()
    )));
    assert!(!key_input::may_be_unbracketed_paste_key(key(
        KeyCode::Char('a'),
        KeyModifiers::ALT
    )));
}
