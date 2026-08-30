//! Who touched what, and telling the people it matters to.
//!
//! The guarantee is **advisory and nothing more**: no lock is taken, no write is
//! blocked, and no change is rolled back. Two agents that edit the same lines
//! both succeed, and both are told. Git is the merge substrate; the task graph
//! is the ordering mechanism; this is the thing that stops two agents finding
//! out at merge time.
//!
//! Saying that plainly matters, because the honest version is far less
//! impressive than "conflict detection" sounds and far more useful than a lock
//! that agents route around.
//!
//! Two policy decisions are made here rather than left implicit:
//!
//! - **Prior readers are alerted, not only prior writers.** An agent that read a
//!   function and is now reasoning about it is *exactly* the agent whose
//!   conclusions just went stale. Alerting only writers optimises for a quiet
//!   log at the cost of the case the mechanism is for. Reader alerts carry a
//!   different, softer wording, because the reader has not lost work — it has
//!   lost currency.
//! - **A touch expires.** An agent that read a file an hour ago and moved on is
//!   not interested. The window is configurable and short by default.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use localpilot_core::SessionId;
use localpilot_harness::{SoftInterrupt, SoftInterruptSource};
use localpilot_tools::touch::{normalise, FileTouch, TouchOp};
use tokio::sync::RwLock;

use super::scope::SwarmId;
use super::spawn::SwarmHost;

/// How long a touch stays interesting.
///
/// Short by default. The cost of forgetting too early is a missed alert on work
/// somebody has probably moved on from; the cost of remembering too long is
/// interrupting agents about files they finished with.
pub const DEFAULT_TTL: Duration = Duration::from_secs(15 * 60);

/// One recorded access.
#[derive(Debug, Clone)]
struct Access {
    session: SessionId,
    touch: FileTouch,
    at: Instant,
}

/// What a landing change means for one peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collision {
    /// The peer to tell.
    pub peer: SessionId,
    /// What that peer had done to the file. Retaining the actual touch, rather
    /// than only its operation, lets a reciprocal alert name that peer's true
    /// path and range.
    pub their_touch: FileTouch,
    /// Whether the two touches actually overlap, rather than merely sharing a
    /// file. The recipient reads a different sentence for each.
    pub overlapping: bool,
}

/// A record of who has touched what recently, and the notifications that fall
/// out of it.
///
/// Cloning is cheap and yields another handle onto the same index.
#[derive(Clone)]
pub struct TouchIndex {
    inner: std::sync::Arc<RwLock<Index>>,
    ttl: Duration,
}

#[derive(Default)]
struct Index {
    /// Keyed by normalised path, so the same file reached as `src/lib.rs` and
    /// `./src/lib.rs` lands in one bucket. Getting this wrong fails *open* —
    /// two agents editing one file, each told nothing.
    by_path: HashMap<String, Vec<Access>>,
}

impl Default for TouchIndex {
    fn default() -> Self {
        Self::new(DEFAULT_TTL)
    }
}

impl TouchIndex {
    /// An empty index with the given expiry window.
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self {
            inner: std::sync::Arc::new(RwLock::new(Index::default())),
            ttl,
        }
    }

    /// Record a touch and return the peers it collides with, most recent first.
    ///
    /// One call rather than record-then-query: the two must see the same state,
    /// and a gap between them is a race in which two simultaneous edits each
    /// record before either queries, and neither is told.
    pub async fn record(
        &self,
        session: SessionId,
        touch: &FileTouch,
        now: Instant,
    ) -> Vec<Collision> {
        let key = normalise(&touch.path);
        let mut guard = self.inner.write().await;
        let ttl = self.ttl;
        let accesses = guard.by_path.entry(key).or_default();
        accesses.retain(|access| now.duration_since(access.at) < ttl);

        let collisions = if touch.op.is_mutation() {
            // Only a *change* starts a conversation. A read that lands on a file
            // someone else read is not news.
            let mut seen: Vec<Collision> = Vec::new();
            for access in accesses.iter().rev() {
                if access.session == session {
                    continue;
                }
                if !access.touch.collides_with(touch) {
                    continue;
                }
                // One alert per peer: the most recent thing they did to this
                // file is the only one they need to hear about.
                if seen.iter().any(|c| c.peer == access.session) {
                    continue;
                }
                seen.push(Collision {
                    peer: access.session,
                    their_touch: access.touch.clone(),
                    overlapping: overlapping(&access.touch, touch),
                });
            }
            seen
        } else {
            Vec::new()
        };

        accesses.push(Access {
            session,
            touch: touch.clone(),
            at: now,
        });
        collisions
    }

    /// Forget everything a session touched. Called when it leaves, so a
    /// departed agent stops generating alerts about files nobody is holding.
    pub async fn forget(&self, session: SessionId) {
        let mut guard = self.inner.write().await;
        for accesses in guard.by_path.values_mut() {
            accesses.retain(|access| access.session != session);
        }
        guard.by_path.retain(|_, accesses| !accesses.is_empty());
    }

    /// Which files a session has touched recently, for a status view.
    pub async fn paths_touched_by(&self, session: SessionId, now: Instant) -> Vec<String> {
        let guard = self.inner.read().await;
        let mut out: Vec<String> = guard
            .by_path
            .iter()
            .filter(|(_, accesses)| {
                accesses.iter().any(|access| {
                    access.session == session && now.duration_since(access.at) < self.ttl
                })
            })
            .map(|(path, _)| path.clone())
            .collect();
        out.sort();
        out
    }

    /// How many live accesses the index holds, for tests and diagnostics.
    pub async fn len(&self, now: Instant) -> usize {
        let guard = self.inner.read().await;
        guard
            .by_path
            .values()
            .flatten()
            .filter(|access| now.duration_since(access.at) < self.ttl)
            .count()
    }

    /// Whether the index holds nothing live.
    pub async fn is_empty(&self, now: Instant) -> bool {
        self.len(now).await == 0
    }
}

/// Whether two touches actually share lines, as opposed to merely sharing a file.
fn overlapping(a: &FileTouch, b: &FileTouch) -> bool {
    match (a.lines, b.lines) {
        (Some(x), Some(y)) => x.overlaps(y),
        _ => true,
    }
}

/// Record a change and tell whoever it affects, over the swarm's own delivery
/// path.
///
/// Returns the peers that were notified. Advisory throughout: nothing here can
/// refuse a write, because the write already happened.
pub async fn announce(
    host: &SwarmHost,
    index: &TouchIndex,
    swarm: &SwarmId,
    session: SessionId,
    touch: &FileTouch,
) -> Vec<SessionId> {
    let collisions = index.record(session, touch, Instant::now()).await;
    if collisions.is_empty() {
        return Vec::new();
    }
    let who = member_name(host, swarm, session).await;

    let mut notified = Vec::new();
    for collision in collisions {
        let Some(peer_host) = host.host(collision.peer).await else {
            continue;
        };
        peer_host.inject(SoftInterrupt {
            content: alert(&who, touch, collision.their_touch.op, collision.overlapping),
            source: SoftInterruptSource::System,
            // Not urgent: this is advice, and cutting a peer's tool batch in
            // half to deliver advice would make the mechanism the problem.
            urgent: false,
        });
        notified.push(collision.peer);

        // A prior writer needs the landing writer's change, and the landing
        // writer equally needs to know about the prior write it overlapped.
        // Readers remain one-directional: their knowledge went stale, while the
        // writer has no lost work to recover. This is topology-agnostic so the
        // shared-worktree guarantee is identical for pairs and hierarchies.
        if collision.their_touch.op.is_mutation() {
            let Some(current_host) = host.host(session).await else {
                continue;
            };
            let peer_name = member_name(host, swarm, collision.peer).await;
            current_host.inject(SoftInterrupt {
                content: alert(
                    &peer_name,
                    &collision.their_touch,
                    touch.op,
                    collision.overlapping,
                ),
                source: SoftInterruptSource::System,
                urgent: false,
            });
        }
    }
    notified
}

async fn member_name(host: &SwarmHost, swarm: &SwarmId, session: SessionId) -> String {
    host.swarms()
        .member(swarm, session)
        .await
        .map_or_else(|| session.to_string(), |member| member.name)
}

/// The sentence a peer reads.
///
/// A writer is told it may have lost work; a reader is told what it knows may be
/// stale. Those are different problems and deserve different sentences — a
/// reader handed "your edit may have been overwritten" would go looking for an
/// edit it never made.
fn alert(who: &str, touch: &FileTouch, theirs: TouchOp, overlapping: bool) -> String {
    let where_ = touch
        .lines
        .map_or_else(|| "the whole file".to_string(), |lines| lines.to_string());
    let path = touch.path.display();
    let scope = if overlapping {
        "overlapping the part you were working on"
    } else {
        "elsewhere in a file you have open"
    };

    if theirs.is_mutation() {
        format!(
            "{who} just {} {path} ({where_}), {scope}. Nothing was locked or rolled back — you \
             both wrote. Re-read the file before your next edit to it, and message {who} if you \
             are both changing the same thing.",
            touch.op.word()
        )
    } else {
        format!(
            "{who} just {} {path} ({where_}), {scope}. What you read from that file may be out of \
             date; re-read it before relying on it.",
            touch.op.word()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_tools::touch::LineRange;

    fn edit(path: &str, from: u32, to: u32) -> FileTouch {
        FileTouch::ranged(path, TouchOp::Modified, LineRange::new(from, to))
    }

    fn read(path: &str, from: u32, to: u32) -> FileTouch {
        FileTouch::ranged(path, TouchOp::Read, LineRange::new(from, to))
    }

    #[tokio::test]
    async fn two_edits_to_the_same_lines_collide() {
        let index = TouchIndex::default();
        let now = Instant::now();
        let alice = SessionId::new();
        let bob = SessionId::new();

        assert!(index
            .record(alice, &edit("src/lib.rs", 10, 20), now)
            .await
            .is_empty());
        let collisions = index.record(bob, &edit("src/lib.rs", 15, 25), now).await;

        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].peer, alice);
        assert!(collisions[0].overlapping);
    }

    #[tokio::test]
    async fn two_edits_far_apart_in_one_file_are_reported_as_the_same_file_not_a_clash() {
        let index = TouchIndex::default();
        let now = Instant::now();
        let alice = SessionId::new();
        let bob = SessionId::new();

        index.record(alice, &edit("src/lib.rs", 1, 20), now).await;
        let collisions = index.record(bob, &edit("src/lib.rs", 400, 420), now).await;

        assert!(
            collisions.is_empty(),
            "non-overlapping ranges in one file are ordinary and must not interrupt anyone"
        );
    }

    #[tokio::test]
    async fn a_whole_file_write_reaches_everyone_working_in_that_file() {
        let index = TouchIndex::default();
        let now = Instant::now();
        let alice = SessionId::new();
        let bob = SessionId::new();

        index
            .record(alice, &edit("src/lib.rs", 400, 420), now)
            .await;
        let collisions = index
            .record(bob, &FileTouch::whole("src/lib.rs", TouchOp::Wrote), now)
            .await;

        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].peer, alice);
        assert!(collisions[0].overlapping);
    }

    #[tokio::test]
    async fn a_prior_reader_is_told_its_knowledge_went_stale() {
        let index = TouchIndex::default();
        let now = Instant::now();
        let reader = SessionId::new();
        let writer = SessionId::new();

        index.record(reader, &read("src/lib.rs", 1, 40), now).await;
        let collisions = index.record(writer, &edit("src/lib.rs", 10, 20), now).await;

        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].their_touch.op, TouchOp::Read);

        let message = alert(
            "writer",
            &edit("src/lib.rs", 10, 20),
            collisions[0].their_touch.op,
            collisions[0].overlapping,
        );
        assert!(message.contains("out of date"), "{message}");
        assert!(
            !message.contains("overwritten"),
            "a reader has not lost work and must not be sent looking for an edit it never made"
        );
    }

    #[tokio::test]
    async fn a_read_landing_on_someone_elses_read_is_not_news() {
        let index = TouchIndex::default();
        let now = Instant::now();
        index
            .record(SessionId::new(), &read("src/lib.rs", 1, 40), now)
            .await;
        let collisions = index
            .record(SessionId::new(), &read("src/lib.rs", 1, 40), now)
            .await;
        assert!(collisions.is_empty());
    }

    #[tokio::test]
    async fn a_session_never_collides_with_itself() {
        let index = TouchIndex::default();
        let now = Instant::now();
        let me = SessionId::new();
        index.record(me, &edit("src/lib.rs", 1, 40), now).await;
        assert!(index
            .record(me, &edit("src/lib.rs", 1, 40), now)
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn one_peer_produces_one_alert_however_many_times_it_touched_the_file() {
        let index = TouchIndex::default();
        let now = Instant::now();
        let alice = SessionId::new();
        for _ in 0..5 {
            index.record(alice, &edit("src/lib.rs", 10, 20), now).await;
        }
        let collisions = index
            .record(SessionId::new(), &edit("src/lib.rs", 10, 20), now)
            .await;
        assert_eq!(collisions.len(), 1, "deduped to the latest touch per peer");
    }

    #[tokio::test]
    async fn a_stale_touch_stops_generating_alerts() {
        let index = TouchIndex::new(Duration::from_secs(60));
        let start = Instant::now();
        let alice = SessionId::new();
        index
            .record(alice, &edit("src/lib.rs", 10, 20), start)
            .await;

        let later = start + Duration::from_secs(61);
        let collisions = index
            .record(SessionId::new(), &edit("src/lib.rs", 10, 20), later)
            .await;
        assert!(collisions.is_empty());
        assert_eq!(index.len(later).await, 1, "only the new touch is live");
    }

    #[tokio::test]
    async fn the_same_file_under_different_spellings_is_one_bucket() {
        let index = TouchIndex::default();
        let now = Instant::now();
        let alice = SessionId::new();
        index
            .record(alice, &edit("./src/lib.rs", 10, 20), now)
            .await;
        let collisions = index
            .record(SessionId::new(), &edit("src/lib.rs", 10, 20), now)
            .await;
        assert_eq!(
            collisions.len(),
            1,
            "path spelling must not be a way to miss a collision"
        );
    }

    #[tokio::test]
    async fn forgetting_a_session_stops_its_alerts() {
        let index = TouchIndex::default();
        let now = Instant::now();
        let gone = SessionId::new();
        index.record(gone, &edit("src/lib.rs", 10, 20), now).await;
        index.forget(gone).await;

        assert!(index
            .record(SessionId::new(), &edit("src/lib.rs", 10, 20), now)
            .await
            .is_empty());
        assert!(index.is_empty(now).await || index.len(now).await == 1);
    }

    #[tokio::test]
    async fn a_sessions_own_recent_paths_are_reportable() {
        let index = TouchIndex::default();
        let now = Instant::now();
        let me = SessionId::new();
        index.record(me, &edit("src/lib.rs", 1, 2), now).await;
        index.record(me, &edit("src/main.rs", 1, 2), now).await;
        index
            .record(SessionId::new(), &edit("other.rs", 1, 2), now)
            .await;

        assert_eq!(
            index.paths_touched_by(me, now).await,
            vec!["src/lib.rs".to_string(), "src/main.rs".to_string()]
        );
    }

    #[test]
    fn a_writers_alert_says_plainly_that_nothing_was_locked() {
        let message = alert(
            "alpha",
            &edit("src/lib.rs", 10, 20),
            TouchOp::Modified,
            true,
        );
        assert!(
            message.contains("Nothing was locked or rolled back"),
            "{message}"
        );
        assert!(message.contains("lines 10-20"), "{message}");
        assert!(message.contains("overlapping"), "{message}");
    }

    #[test]
    fn a_non_overlapping_alert_says_so_rather_than_implying_a_clash() {
        let message = alert(
            "alpha",
            &edit("src/lib.rs", 10, 20),
            TouchOp::Modified,
            false,
        );
        assert!(
            message.contains("elsewhere in a file you have open"),
            "{message}"
        );
    }
}
