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
    /// The byte size each entry was pushed with (see [`SeqRing::push_sized`]).
    sizes: VecDeque<usize>,
    capacity: usize,
    /// The most bytes the entries may hold together (`usize::MAX`: no budget).
    max_bytes: usize,
    bytes: usize,
    next_seq: u64,
    dropped: u64,
}

impl<T: Clone> SeqRing<T> {
    /// An empty ring keeping at most `capacity` entries (at least one).
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self::with_byte_budget(capacity, usize::MAX)
    }

    /// An empty ring keeping at most `capacity` entries and, counting the sizes
    /// given to [`SeqRing::push_sized`], at most `max_bytes` bytes. An entry
    /// larger than the whole budget still takes its sequence number but is not
    /// kept, so a reader behind it sees an incomplete range (and takes a
    /// snapshot) instead of the ring holding it.
    #[must_use]
    pub fn with_byte_budget(capacity: usize, max_bytes: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            sizes: VecDeque::new(),
            capacity: capacity.max(1),
            max_bytes,
            bytes: 0,
            next_seq: 1,
            dropped: 0,
        }
    }

    /// Append `item`, returning its sequence number.
    pub fn push(&mut self, item: T) -> u64 {
        self.push_sized(item, 0)
    }

    /// Append `item` counting `size` bytes against the byte budget, returning
    /// its sequence number. The oldest entries go first.
    pub fn push_sized(&mut self, item: T, size: usize) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.push_back((seq, item));
        self.sizes.push_back(size);
        self.bytes = self.bytes.saturating_add(size);
        while self.entries.len() > self.capacity
            || (self.bytes > self.max_bytes && !self.entries.is_empty())
        {
            self.entries.pop_front();
            self.bytes = self
                .bytes
                .saturating_sub(self.sizes.pop_front().unwrap_or(0));
            self.dropped += 1;
        }
        seq
    }

    /// Whether this ring counts bytes.
    #[must_use]
    pub fn has_byte_budget(&self) -> bool {
        self.max_bytes != usize::MAX
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
        self.sizes.clear();
        self.bytes = 0;
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

    #[test]
    fn a_byte_budget_drops_old_entries_and_never_keeps_one_larger_than_itself() {
        let mut ring = SeqRing::with_byte_budget(100, 10);
        ring.push_sized("a", 4);
        ring.push_sized("b", 4);
        ring.push_sized("c", 4); // 12 > 10: `a` goes
        assert_eq!(ring.complete_after(1).map(|v| v.len()), Some(2));
        assert_eq!(ring.complete_after(0), None, "the dropped entry is a gap");
        let big = ring.push_sized("huge", 11);
        assert_eq!(big, 4, "it still takes its number");
        assert!(ring.after(0).all(|(seq, _)| *seq != big), "but is not kept");
        assert_eq!(
            ring.complete_after(3),
            None,
            "a reader behind it must resync"
        );
        assert_eq!(ring.complete_after(4).map(|v| v.len()), Some(0));
    }
}
