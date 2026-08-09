use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// How long after a burst key a following key still counts as part of the same
/// paste, and how long with no input before a pending burst is committed. A real
/// paste never pauses this long mid-stream, so it is committed as one block;
/// ordinary typing never enters a burst, so this never delays it.
const PASTE_BURST_WINDOW: Duration = Duration::from_millis(150);
/// A legacy key-record paste presents several records effectively at once.
/// Requiring a short dense prefix before treating Enter as paste content keeps
/// scheduler-bunched human typing from swallowing the submit key.
const PASTE_DENSE_WINDOW: Duration = Duration::from_millis(5);
const PASTE_DENSE_PREFIX: usize = 3;

pub(crate) fn is_key_action(key: KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
}

/// What the input loop should do with a key while watching for an *unbracketed*
/// paste — text that arrives as a rapid stream of key events because the terminal
/// did not deliver a single bracketed `Event::Paste`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PasteAction {
    /// Not part of a burst; handle the key through the normal chain.
    Pass,
    /// The key was absorbed into the in-progress burst; nothing else to do.
    Absorbed,
    /// The burst ended with this key; insert the accumulated text as a paste.
    Flush(String),
    /// The burst ended *before* this key; insert the accumulated text, then still
    /// handle this key through the normal chain.
    FlushThenPass(String),
}

/// Accumulates a run of key events that look like pasted text — each arriving
/// with more input already queued, or within [`PASTE_BURST_WINDOW`] of the
/// previous — so a terminal without bracketed paste still collapses a large paste
/// to a placeholder instead of dumping every line into the composer.
#[derive(Debug, Default)]
pub(crate) struct PasteBurst {
    buffer: String,
    active_until: Option<Instant>,
    dense_started_at: Option<Instant>,
    dense_records: usize,
    multiline_confirmed: bool,
    pending_cr: bool,
    bracketed_paste_seen: bool,
}

impl PasteBurst {
    /// Whether a burst is mid-accumulation.
    pub(crate) fn has_pending(&self) -> bool {
        !self.buffer.is_empty()
    }

    /// A terminal that has emitted `Event::Paste` supports bracketed paste for
    /// this session. Its later key events are therefore human input, never the
    /// legacy unbracketed fallback.
    pub(crate) fn unbracketed_enabled(&self) -> bool {
        !self.bracketed_paste_seen
    }

    /// Permanently retire the legacy heuristic after the first real bracketed
    /// paste. Any staged prefix is returned so its ordering can be preserved.
    pub(crate) fn note_bracketed_paste(&mut self) -> Option<String> {
        self.bracketed_paste_seen = true;
        self.flush_pending()
    }

    /// Commit an in-progress burst once it has gone idle — no new input for a full
    /// [`PASTE_BURST_WINDOW`]. This is what registers a paste whose final
    /// character was *absorbed* rather than flushed (a trailing event, e.g. a
    /// key-release report, looked like more input was coming). Returns `None`
    /// while the burst is still live, so a momentary gap mid-paste does not commit
    /// a half-paste.
    pub(crate) fn flush_if_idle(&mut self, now: Instant) -> Option<String> {
        let idle = self.active_until.is_some_and(|until| now > until);
        (self.has_pending() && idle).then(|| self.take())
    }

    /// Commit a pending burst immediately when the input owner changes (for
    /// example, when a tool dialog takes focus). This keeps text already typed
    /// for the composer out of the newly opened dialog.
    pub(crate) fn flush_pending(&mut self) -> Option<String> {
        self.has_pending().then(|| self.take())
    }

    fn take(&mut self) -> String {
        self.active_until = None;
        self.dense_started_at = None;
        self.dense_records = 0;
        self.multiline_confirmed = false;
        self.pending_cr = false;
        std::mem::take(&mut self.buffer)
    }

    fn record_dense_key(&mut self, now: Instant) {
        let started = self.dense_started_at.get_or_insert(now);
        if now.saturating_duration_since(*started) <= PASTE_DENSE_WINDOW {
            self.dense_records = self.dense_records.saturating_add(1);
        }
    }

    /// Append one character, normalizing `\r` and `\r\n` to a single `\n` so the
    /// row count and the expanded text are clean regardless of line endings.
    fn push(&mut self, c: char) {
        match c {
            '\r' => {
                self.buffer.push('\n');
                self.pending_cr = true;
            }
            // The LF half of a CRLF: the newline was already emitted on the CR.
            '\n' if self.pending_cr => self.pending_cr = false,
            _ => {
                self.buffer.push(c);
                self.pending_cr = false;
            }
        }
    }

    pub(crate) fn observe(
        &mut self,
        key: KeyEvent,
        buffered_after: bool,
        now: Instant,
    ) -> PasteAction {
        if !self.unbracketed_enabled() {
            return if self.has_pending() {
                PasteAction::FlushThenPass(self.take())
            } else {
                PasteAction::Pass
            };
        }
        let Some(c) = paste_char(key) else {
            return if self.has_pending() {
                PasteAction::FlushThenPass(self.take())
            } else {
                PasteAction::Pass
            };
        };

        let in_burst = buffered_after || self.active_until.is_some_and(|until| now <= until);
        if matches!(c, '\n' | '\r') && !self.multiline_confirmed {
            if !self.has_pending() {
                return PasteAction::Pass;
            }
            let dense_prefix = self.dense_started_at.is_some_and(|started| {
                now.saturating_duration_since(started) <= PASTE_DENSE_WINDOW
                    && self.dense_records >= PASTE_DENSE_PREFIX
            });
            if !(buffered_after && dense_prefix) {
                // An Enter that ends a bunched run is still the user's submit.
                // Flush the staged text, but let Enter follow the normal path.
                return PasteAction::FlushThenPass(self.take());
            }
            self.multiline_confirmed = true;
        }
        if in_burst {
            self.record_dense_key(now);
            self.push(c);
            self.active_until = Some(now + PASTE_BURST_WINDOW);
            if buffered_after {
                PasteAction::Absorbed
            } else {
                // Last key of the batch: the burst is complete.
                PasteAction::Flush(self.take())
            }
        } else if self.has_pending() {
            PasteAction::FlushThenPass(self.take())
        } else {
            PasteAction::Pass
        }
    }
}

pub(crate) fn may_be_unbracketed_paste_key(key: KeyEvent) -> bool {
    paste_char(key).is_some()
}

pub(crate) fn is_unbracketed_paste_newline_key(key: KeyEvent) -> bool {
    matches!(paste_char(key), Some('\n' | '\r'))
}

pub(crate) fn is_cancel(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL))
}

/// Ctrl+V — the request to attach an image from the OS clipboard. Terminals that
/// translate Ctrl+V into a bracketed paste are handled separately (an empty paste
/// also probes the clipboard).
pub(crate) fn is_clipboard_image_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL))
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

/// The text character a key contributes when it is paste content, or `None` for a
/// command key. SHIFT is allowed — pasted capitals and shifted punctuation are
/// text, and the kitty keyboard protocol reports SHIFT for them — but CTRL / ALT /
/// SUPER mark commands, not text.
fn paste_char(key: KeyEvent) -> Option<char> {
    if !key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
        return None;
    }
    match key.code {
        KeyCode::Char(c) => Some(c),
        KeyCode::Enter => Some('\n'),
        _ => None,
    }
}
