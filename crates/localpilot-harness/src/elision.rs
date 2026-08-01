//! Already-seen `read_file` elision.
//!
//! Tracks, per session, which `(path, line-range)` reads have been served and the
//! file's mtime + length at that time. When the model reads the exact same
//! `(path, range)` again and the file is unchanged (same mtime *and* length), the
//! harness can return a compact stub instead of the full body, saving context on
//! read-heavy loops. Conservative by construction: a changed file — or any doubt
//! (a coarse-mtime same-length overwrite, an unreadable stat) — is never elided,
//! so the model can never be handed stale content. Lost on resume (in-memory), so
//! a resumed session simply re-serves full content until it re-reads.

use std::collections::HashMap;

/// Cap on distinct tracked reads. A session reading past this many distinct
/// `(path, range)` pairs clears the map and stops eliding until it refills — a
/// bound, never a correctness risk (clearing only forgoes elision).
const MAX_TRACKED_READS: usize = 2048;

/// The identity of one read: the normalized path and its requested line range
/// (`None` = the whole file). Elision is exact — a different range is a different
/// read, not "already seen".
type ReadKey = (String, Option<usize>, Option<usize>);

/// The freshness baseline captured when a read was served.
#[derive(Debug, Clone)]
struct Seen {
    mtime_unix: u64,
    len: u64,
    call_id: String,
}

/// Per-session record of served reads and their freshness baselines.
#[derive(Debug, Default)]
pub(crate) struct ReadHistory {
    seen: HashMap<ReadKey, Seen>,
}

impl ReadHistory {
    /// If this exact `(path, range)` was already read and the file is unchanged
    /// since (same mtime and length), return the earlier call's id so the caller
    /// can cite it in the stub. Any mismatch — or a caller that has not recorded
    /// this read yet — returns `None`, so full content is served.
    pub(crate) fn elidable(
        &self,
        path: &str,
        start: Option<usize>,
        end: Option<usize>,
        current_mtime: u64,
        current_len: u64,
    ) -> Option<String> {
        let seen = self.seen.get(&(path.to_string(), start, end))?;
        (seen.mtime_unix == current_mtime && seen.len == current_len).then(|| seen.call_id.clone())
    }

    /// Record that `(path, range)` was served for `call_id` with the given
    /// freshness baseline. Overwrites any prior baseline for the same read (the
    /// latest is what a future read is compared against).
    pub(crate) fn record(
        &mut self,
        path: &str,
        start: Option<usize>,
        end: Option<usize>,
        mtime_unix: u64,
        len: u64,
        call_id: &str,
    ) {
        if self.seen.len() >= MAX_TRACKED_READS
            && !self.seen.contains_key(&(path.to_string(), start, end))
        {
            self.seen.clear();
        }
        self.seen.insert(
            (path.to_string(), start, end),
            Seen {
                mtime_unix,
                len,
                call_id: call_id.to_string(),
            },
        );
    }

    /// Forget every read of `path` (e.g. after a write to it), so a later read is
    /// served in full. Belt-and-suspenders — the mtime/length check already
    /// catches an ordinary write — but robust against a coarse-mtime overwrite.
    pub(crate) fn forget_path(&mut self, path: &str) {
        self.seen.retain(|(p, _, _), _| p != path);
    }
}

/// The stub returned in place of an already-seen, unchanged read. Names the file,
/// cites the earlier read, and points the model at the ways to get content again,
/// so eliding never hides data — it defers it.
#[must_use]
pub(crate) fn elision_stub(path: &str, prior_id: &str, elided_bytes: usize) -> String {
    format!(
        "read_file({path}) — unchanged since it was read earlier this turn/session \
         (call {prior_id}); {elided_bytes} bytes elided to save context. Re-read a \
         specific range with read_file start_line/end_line if you need it again."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exact_unchanged_reread_is_elidable_a_changed_one_is_not() {
        let mut h = ReadHistory::default();
        // Not recorded yet: never elidable.
        assert!(h.elidable("a.rs", None, None, 100, 50).is_none());

        h.record("a.rs", None, None, 100, 50, "c1");
        // Same read, unchanged file: elidable, citing the earlier call.
        assert_eq!(
            h.elidable("a.rs", None, None, 100, 50).as_deref(),
            Some("c1")
        );
        // Changed mtime: never elided (would be stale).
        assert!(h.elidable("a.rs", None, None, 101, 50).is_none());
        // Same mtime, different length (a coarse-mtime overwrite): never elided.
        assert!(h.elidable("a.rs", None, None, 100, 60).is_none());
        // A different range is a different read: not "already seen".
        assert!(h.elidable("a.rs", Some(1), Some(10), 100, 50).is_none());
    }

    #[test]
    fn forgetting_a_path_serves_it_full_again() {
        let mut h = ReadHistory::default();
        h.record("a.rs", None, None, 100, 50, "c1");
        h.forget_path("a.rs");
        assert!(h.elidable("a.rs", None, None, 100, 50).is_none());
    }

    #[test]
    fn a_range_read_is_keyed_independently() {
        let mut h = ReadHistory::default();
        h.record("a.rs", Some(1), Some(20), 100, 50, "c1");
        assert_eq!(
            h.elidable("a.rs", Some(1), Some(20), 100, 50).as_deref(),
            Some("c1")
        );
        // A wider range was never served: not elidable off the narrower one.
        assert!(h.elidable("a.rs", None, None, 100, 50).is_none());
    }
}
