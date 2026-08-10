#![allow(clippy::unwrap_used)]

#[path = "../src/key_input.rs"]
mod key_input;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::time::{Duration, Instant};

fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
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
fn ctrl_v_requests_a_clipboard_image() {
    assert!(key_input::is_clipboard_image_key(key(
        KeyCode::Char('v'),
        KeyModifiers::CONTROL
    )));
    // A bare 'v' is ordinary text input, not an image request.
    assert!(!key_input::is_clipboard_image_key(key(
        KeyCode::Char('v'),
        KeyModifiers::empty()
    )));
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

fn plain(c: char) -> KeyEvent {
    key(KeyCode::Char(c), KeyModifiers::empty())
}

#[test]
fn a_key_burst_is_absorbed_then_flushed_as_one_paste() {
    let now = Instant::now();
    let mut burst = key_input::PasteBurst::default();

    // 'a' and 'b' arrive with more queued; 'c' is the last of the batch.
    assert_eq!(
        burst.observe(plain('a'), true, now),
        key_input::PasteAction::Absorbed
    );
    assert_eq!(
        burst.observe(plain('b'), true, now + Duration::from_millis(1)),
        key_input::PasteAction::Absorbed
    );
    assert_eq!(
        burst.observe(plain('c'), false, now + Duration::from_millis(2)),
        key_input::PasteAction::Flush("abc".to_string())
    );
}

#[test]
fn a_bunched_human_run_flushes_then_lets_enter_submit() {
    let now = Instant::now();
    let mut burst = key_input::PasteBurst::default();

    burst.observe(plain('a'), true, now);
    // One buffered character is not enough evidence to absorb Enter. Preserve
    // the staged text, then let the normal input path submit it.
    assert_eq!(
        burst.observe(
            key(KeyCode::Enter, KeyModifiers::empty()),
            false,
            now + Duration::from_millis(1)
        ),
        key_input::PasteAction::FlushThenPass("a".to_string())
    );
}

#[test]
fn crlf_in_a_burst_is_normalized_to_one_newline() {
    let now = Instant::now();
    let mut burst = key_input::PasteBurst::default();

    burst.observe(plain('a'), true, now);
    burst.observe(plain('b'), true, now + Duration::from_millis(1));
    burst.observe(plain('c'), true, now + Duration::from_millis(2));
    burst.observe(plain('\r'), true, now + Duration::from_millis(3));
    burst.observe(plain('\n'), true, now + Duration::from_millis(4));
    assert_eq!(
        burst.observe(plain('d'), false, now + Duration::from_millis(5)),
        key_input::PasteAction::Flush("abc\nd".to_string())
    );
}

#[test]
fn bracketed_paste_permanently_disables_the_legacy_key_heuristic() {
    let now = Instant::now();
    let mut burst = key_input::PasteBurst::default();
    assert_eq!(burst.note_bracketed_paste(), None);
    assert!(!burst.unbracketed_enabled());
    assert_eq!(
        burst.observe(plain('a'), true, now),
        key_input::PasteAction::Pass
    );
    assert_eq!(
        burst.observe(
            key(KeyCode::Enter, KeyModifiers::empty()),
            true,
            now + Duration::from_millis(1)
        ),
        key_input::PasteAction::Pass
    );
}

#[test]
fn a_lone_keystroke_passes_through_unbuffered() {
    let now = Instant::now();
    let mut burst = key_input::PasteBurst::default();

    // Nothing queued after and no active burst: ordinary typing, handled normally.
    assert_eq!(
        burst.observe(plain('a'), false, now),
        key_input::PasteAction::Pass
    );
    assert!(!burst.has_pending());
}

#[test]
fn shifted_characters_are_paste_content() {
    let now = Instant::now();
    let mut burst = key_input::PasteBurst::default();

    // The kitty keyboard protocol reports SHIFT for capitals and shifted
    // punctuation; they must still accumulate into the paste, not break it.
    let shift_l = key(KeyCode::Char('L'), KeyModifiers::SHIFT);
    let shift_paren = key(KeyCode::Char('('), KeyModifiers::SHIFT);
    assert!(key_input::may_be_unbracketed_paste_key(shift_l));

    assert_eq!(
        burst.observe(shift_l, true, now),
        key_input::PasteAction::Absorbed
    );
    assert_eq!(
        burst.observe(plain('o'), true, now + Duration::from_millis(1)),
        key_input::PasteAction::Absorbed
    );
    assert_eq!(
        burst.observe(shift_paren, false, now + Duration::from_millis(2)),
        key_input::PasteAction::Flush("Lo(".to_string())
    );
}

#[test]
fn ctrl_and_alt_keys_are_not_paste_content() {
    assert!(!key_input::may_be_unbracketed_paste_key(key(
        KeyCode::Char('a'),
        KeyModifiers::CONTROL
    )));
    assert!(!key_input::may_be_unbracketed_paste_key(key(
        KeyCode::Char('a'),
        KeyModifiers::ALT
    )));
}

#[test]
fn an_absorbed_burst_is_committed_only_after_it_goes_idle() {
    let now = Instant::now();
    let mut burst = key_input::PasteBurst::default();

    // Every key reports more buffered after it (e.g. a trailing key-release), so
    // even the final character is absorbed rather than flushed.
    burst.observe(plain('a'), true, now);
    burst.observe(plain('b'), true, now + Duration::from_millis(1));
    assert!(burst.has_pending());

    // A momentary gap mid-paste must not commit a half: still live just after.
    assert_eq!(
        burst.flush_if_idle(now + Duration::from_millis(2)),
        None,
        "a brief gap should not commit the burst"
    );

    // Once no input has arrived for the full window, the burst commits.
    assert_eq!(
        burst.flush_if_idle(now + Duration::from_secs(1)),
        Some("ab".to_string())
    );
    assert!(!burst.has_pending());
    assert_eq!(burst.flush_if_idle(now + Duration::from_secs(2)), None);
}

#[test]
fn a_pending_burst_can_be_committed_when_input_focus_changes() {
    let now = Instant::now();
    let mut burst = key_input::PasteBurst::default();

    burst.observe(plain('a'), true, now);
    burst.observe(plain('b'), true, now + Duration::from_millis(1));
    assert_eq!(burst.flush_pending(), Some("ab".to_string()));
    assert!(!burst.has_pending());
    assert_eq!(burst.flush_pending(), None);
}

#[test]
fn a_non_text_key_flushes_a_pending_burst_then_passes() {
    let now = Instant::now();
    let mut burst = key_input::PasteBurst::default();

    burst.observe(plain('a'), true, now);
    assert_eq!(
        burst.observe(
            key(KeyCode::Left, KeyModifiers::empty()),
            false,
            now + Duration::from_millis(1)
        ),
        key_input::PasteAction::FlushThenPass("a".to_string())
    );
}

#[test]
fn only_unmodified_chars_are_unbracketed_paste_candidates() {
    assert!(key_input::may_be_unbracketed_paste_key(key(
        KeyCode::Char('a'),
        KeyModifiers::empty()
    )));
    assert!(key_input::may_be_unbracketed_paste_key(key(
        KeyCode::Enter,
        KeyModifiers::empty()
    )));
    assert!(!key_input::may_be_unbracketed_paste_key(key(
        KeyCode::Char('a'),
        KeyModifiers::ALT
    )));
}
