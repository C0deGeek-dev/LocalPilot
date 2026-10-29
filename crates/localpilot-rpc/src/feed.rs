//! A bounded, monotonically numbered ring of records.
//!
//! The MCP adapter pages its event feed from one, and the IPC server keeps one
//! per hosted session so a reconnecting client can replay what it missed.
//! Sequence numbers start at 1 and never repeat within a ring's lifetime;
//! overflow drops the oldest entries and counts them, so a reader can always
//! tell whether the entries after its cursor are still complete.

use std::collections::VecDeque;

/// A bounded ring of `(seq, T)` entries.
#[derive(Debug)]
pub struct SeqRing<T> {
    entries: VecDeque<(u64, T)>,
    capacity: usize,
    next_seq: u64,
    dropped: u64,
}

impl<T: Clone> SeqRing<T> {
    /// An empty ring keeping at most `capacity` entries (at least one).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            capacity: capacity.max(1),
            next_seq: 1,
            dropped: 0,
        }
    }

    /// Append `item`, returning its sequence number.
    pub fn push(&mut self, item: T) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.push_back((seq, item));
        while self.entries.len() > self.capacity {
            self.entries.pop_front();
            self.dropped += 1;
        }
        seq
    }

    /// The highest sequence number handed out so far (0 when none).
    #[must_use]
    pub fn head(&self) -> u64 {
        self.next_seq - 1
    }

    /// How many entries overflow has dropped.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// The entries after `cursor`, oldest first. May be incomplete when
    /// overflow dropped some of them; see [`complete_after`](Self::complete_after).
    pub fn after(&self, cursor: u64) -> impl Iterator<Item = &(u64, T)> {
        self.entries.iter().filter(move |(seq, _)| *seq > cursor)
    }

    /// Every entry after `cursor`, or `None` when the ring no longer holds all
    /// of them (or `cursor` is ahead of the head).
    #[must_use]
    pub fn complete_after(&self, cursor: u64) -> Option<Vec<(u64, T)>> {
        if cursor > self.head() {
            return None;
        }
        let oldest_held = self.entries.front().map_or(self.next_seq, |(seq, _)| *seq);
        if cursor + 1 < oldest_held {
            return None;
        }
        Some(self.after(cursor).cloned().collect())
    }

    /// Drop every entry, keeping the sequence numbering.
    pub fn clear(&mut self) {
        self.dropped += self.entries.len() as u64;
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_entries_and_replays_after_a_cursor() {
        let mut ring = SeqRing::new(4);
        assert_eq!(ring.head(), 0);
        for n in 1..=3 {
            assert_eq!(ring.push(n * 10), n);
        }
        assert_eq!(ring.complete_after(1).unwrap(), vec![(2, 20), (3, 30)]);
        assert_eq!(ring.complete_after(3).unwrap(), Vec::new());
        assert!(
            ring.complete_after(4).is_none(),
            "a cursor ahead of the head"
        );
    }

    #[test]
    fn overflow_makes_an_old_cursor_incomplete() {
        let mut ring = SeqRing::new(2);
        for n in 1..=5 {
            ring.push(n);
        }
        assert_eq!(ring.dropped(), 3);
        assert!(ring.complete_after(1).is_none(), "entries 2 and 3 are gone");
        assert_eq!(ring.complete_after(3).unwrap(), vec![(4, 4), (5, 5)]);
    }

    #[test]
    fn clearing_keeps_numbering_and_breaks_old_cursors() {
        let mut ring = SeqRing::new(4);
        ring.push('a');
        ring.push('b');
        ring.clear();
        assert_eq!(ring.push('c'), 3);
        assert!(ring.complete_after(1).is_none());
        assert_eq!(ring.complete_after(2).unwrap(), vec![(3, 'c')]);
    }
}
