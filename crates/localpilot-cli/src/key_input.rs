use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// How long after a burst key a following key still counts as part of the same
/// paste, and how long with no input before a pending burst is committed. A real
/// paste never pauses this long mid-stream, so it is committed as one block;
/// ordinary typing never enters a burst, so this never delays it.
const PASTE_BURST_WINDOW: Duration = Duration::from_millis(150);

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
///
/// Two pieces of state live here and end at different moments. The *staged
/// text* (`buffer`) is handed to the composer as soon as the input queue drains
/// or the burst goes idle. The *burst itself* (`active_until`,
/// `multiline_confirmed`) stays live for the whole continuation window, across
/// those flushes, so a paste that reaches the console in several chunks is still
/// classified once: an Enter early in a later chunk is paste content, not the
/// user's submit. Only idle expiry, a command key, a real bracketed paste, or a
/// focus change end the burst.
#[derive(Debug, Default)]
pub(crate) struct PasteBurst {
    buffer: String,
    active_until: Option<Instant>,
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
    /// a half-paste. Idle expiry also ends the burst, so the next key starts a
    /// fresh classification.
    pub(crate) fn flush_if_idle(&mut self, now: Instant) -> Option<String> {
        let idle = self.active_until.is_some_and(|until| now > until);
        if !idle {
            return None;
        }
        self.end_and_take()
    }

    /// Commit a pending burst immediately when the input owner changes (for
    /// example, when a tool dialog takes focus). This keeps text already typed
    /// for the composer out of the newly opened dialog, and ends the burst.
    pub(crate) fn flush_pending(&mut self) -> Option<String> {
        self.end_and_take()
    }

    /// Hand over the staged text without ending the burst: the queue drained,
    /// but the continuation window (and any multi-line confirmation) still holds
    /// for input that arrives inside it.
    fn take_text(&mut self) -> Option<String> {
        self.has_pending().then(|| std::mem::take(&mut self.buffer))
    }

    /// Forget the burst classification: the next key is judged from scratch.
    fn end_burst(&mut self) {
        self.active_until = None;
        self.multiline_confirmed = false;
        self.pending_cr = false;
    }

    /// End the burst and hand over whatever was staged, for the paths where the
    /// current key is a command (or a human submit) that closes the run.
    fn end_and_take(&mut self) -> Option<String> {
        self.end_burst();
        self.take_text()
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

    /// Classify one key press. `buffered_after` reports whether more input was
    /// already queued behind this key when it was read; `now` is the processing
    /// time. Only `buffered_after` and the continuation window drive the
    /// decision — never how long the run took to *process*: a frame is drawn
    /// between keys (or between batches of keys), so a long first line would
    /// push its Enter outside any wall-clock density window and turn it into a
    /// submit.
    pub(crate) fn observe(
        &mut self,
        key: KeyEvent,
        buffered_after: bool,
        now: Instant,
    ) -> PasteAction {
        if !self.unbracketed_enabled() {
            return match self.end_and_take() {
                Some(text) => PasteAction::FlushThenPass(text),
                None => PasteAction::Pass,
            };
        }
        let Some(c) = paste_char(key) else {
            // A command key closes the run: staged text goes to the composer and
            // the key follows the normal chain.
            return match self.end_and_take() {
                Some(text) => PasteAction::FlushThenPass(text),
                None => PasteAction::Pass,
            };
        };

        let live = self.active_until.is_some_and(|until| now <= until);
        if !live {
            // The continuation window lapsed without an idle flush (or this is
            // the first key ever): whatever follows is a fresh run.
            self.multiline_confirmed = false;
        }
        let in_burst = buffered_after || live;
        if matches!(c, '\n' | '\r') {
            let content = if self.multiline_confirmed {
                // Inside a confirmed multi-line run, Enter is content while text
                // is still staged or more input is queued behind it. An Enter
                // that arrives alone after the staged text was already handed
                // over has no paste evidence left: it is the user's submit, even
                // inside the continuation window.
                buffered_after || self.has_pending()
            } else {
                // The first Enter of a run is paste content only when the run is
                // already under way (text staged, or the window still open)
                // *and* more input is queued behind the Enter — a paste always
                // has both; a human submit has neither unless the UI stalled
                // across the whole char+Enter+char sequence.
                buffered_after && (self.has_pending() || live)
            };
            if !content {
                // An Enter that ends a bunched run is still the user's submit.
                // Flush the staged text, but let Enter follow the normal path.
                return match self.end_and_take() {
                    Some(text) => PasteAction::FlushThenPass(text),
                    None => PasteAction::Pass,
                };
            }
            self.multiline_confirmed = true;
        }
        if in_burst {
            self.push(c);
            self.active_until = Some(now + PASTE_BURST_WINDOW);
            if buffered_after {
                PasteAction::Absorbed
            } else {
                // Last key of the batch: hand the staged text over now, but keep
                // the burst live — a paste that reaches the console in chunks
                // continues inside the window and must not be reclassified.
                match self.take_text() {
                    Some(text) => PasteAction::Flush(text),
                    None => PasteAction::Absorbed,
                }
            }
        } else {
            match self.end_and_take() {
                Some(text) => PasteAction::FlushThenPass(text),
                None => PasteAction::Pass,
            }
        }
    }
}

pub(crate) fn may_be_unbracketed_paste_key(key: KeyEvent) -> bool {
    paste_char(key).is_some()
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
