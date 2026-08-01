//! What happens when a worker stops answering.
//!
//! This is where the engineering weight is. Spawning agents is easy; the hard
//! part is that one of them will die holding an assignment, and everything
//! waiting on that assignment will wait forever unless something notices.
//!
//! Four things have to happen, in order, and each has a way of being subtly
//! wrong:
//!
//! 1. **Notice.** Heartbeats, with staleness measured from the last one — never
//!    from admission, or every worker is reaped the instant it starts.
//! 2. **Salvage.** The corpse's non-terminal assignments go back into the plan,
//!    bounded by a reclaim counter. Unbounded requeuing turns one bad task into
//!    a plan that never finishes and never says why.
//! 3. **Repair the tree.** Children are reparented onto the nearest surviving
//!    ancestor, and a departed coordinator is replaced by a deterministic
//!    successor, so every observer agrees without coordinating.
//! 4. **Say so.** A salvage report goes to whoever now owns the work. A plan
//!    that silently re-runs a task is indistinguishable from one that is stuck.
//!
//! And underneath all of it, **snapshots**: a swarm's plan and membership
//! survive a server restart, in their own stream, so recovering a plan never
//! requires replaying a session transcript.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use localpilot_core::SessionId;
use localpilot_harness::{SoftInterrupt, SoftInterruptSource};
use localpilot_taskgraph::ops::{salvage_actor, Salvage};
use localpilot_taskgraph::{ActorId, TaskPlan};
use serde::{Deserialize, Serialize};

use super::registry::{MemberStatus, SwarmMember};
use super::scope::SwarmId;
use super::spawn::SwarmHost;

/// How long a member may go unheard-from before it is presumed gone.
///
/// Generous relative to a heartbeat interval: a false death is expensive (work
/// is requeued and possibly done twice), while a late detection costs only
/// latency on a plan that is already stalled.
pub const DEFAULT_STALE_AFTER: Duration = Duration::from_secs(90);

/// How many times one task may be salvaged before it is failed instead.
///
/// A task that keeps outliving its workers is the task's problem. Requeuing it
/// forever is the failure mode this number exists to stop.
pub const DEFAULT_RECLAIM_LIMIT: u32 = 2;

/// What a salvage did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Salvaged {
    /// The member that was presumed gone.
    pub departed: SessionId,
    /// Its tasks, and what happened to each.
    pub tasks: Vec<(localpilot_taskgraph::NodeId, Salvage)>,
    /// Children handed to a new parent.
    pub reparented: Vec<SessionId>,
    /// The newly elected coordinator, if the departed member held the seat.
    pub new_coordinator: Option<SessionId>,
    /// Who was told.
    pub reported_to: Option<SessionId>,
}

/// Presume `session` gone and put the swarm back in order.
///
/// Idempotent: salvaging an already-departed member finds no active assignments
/// and no children to move, so a sweep that races another sweep does no harm.
pub async fn salvage(
    host: &SwarmHost,
    swarm: &SwarmId,
    session: SessionId,
    reclaim_limit: u32,
) -> Salvaged {
    let swarms = host.swarms();
    let was_coordinator = swarms.is_coordinator(swarm, session).await;
    let name = swarms
        .member(swarm, session)
        .await
        .map_or_else(|| session.to_string(), |member| member.name);

    let _ = swarms
        .set_status(swarm, session, MemberStatus::Departed)
        .await;

    // Take the corpse's work back before touching the tree: the plan is what
    // everything else is waiting on.
    let tasks = swarms
        .with_plan(swarm, |plan| {
            salvage_actor(plan, &ActorId::new(session.to_string()), reclaim_limit)
        })
        .await
        .unwrap_or_default();

    let reparented = swarms.reparent_children(swarm, session).await;
    let new_coordinator = if was_coordinator {
        swarms.elect_coordinator(swarm).await
    } else {
        None
    };

    // Report to whoever now owns the work: the new coordinator if there is one,
    // otherwise the current one. Reporting to the departed member's parent would
    // be right in a shallower tree and wrong in a deep one.
    let owner = match new_coordinator {
        Some(elected) => Some(elected),
        None => swarms.coordinator(swarm).await,
    }
    .filter(|owner| *owner != session);

    let reported_to = match owner {
        Some(owner) => match host.host(owner).await {
            Some(owner_host) => {
                owner_host.inject(SoftInterrupt {
                    content: salvage_report(&name, session, &tasks, &reparented),
                    source: SoftInterruptSource::System,
                    urgent: false,
                });
                Some(owner)
            }
            None => None,
        },
        None => None,
    };

    // Its file touches go too: a departed agent must not keep generating
    // conflict alerts about files nobody is holding.
    host.touches().forget(session).await;

    Salvaged {
        departed: session,
        tasks,
        reparented,
        new_coordinator,
        reported_to,
    }
}

/// Sweep a swarm for members that have stopped answering, and salvage each.
///
/// Returns what it salvaged, in id order.
pub async fn sweep(
    host: &SwarmHost,
    swarm: &SwarmId,
    stale_after: Duration,
    reclaim_limit: u32,
    now: Instant,
) -> Vec<Salvaged> {
    let stale = host.swarms().stale_members(swarm, stale_after, now).await;
    let mut out = Vec::new();
    for session in stale {
        out.push(salvage(host, swarm, session, reclaim_limit).await);
    }
    out
}

/// Stop hosting members that have reached a terminal state and have no children
/// left waiting on them.
///
/// Returns the sessions reaped. Their *membership records* stay: a coordinator
/// reading the plan later still needs to know what a finished worker reported.
/// What is released is the hosting — the runtime, the event broadcast, the
/// subscriber task.
pub async fn reap_terminal(host: &SwarmHost, swarm: &SwarmId) -> Vec<SessionId> {
    let swarms = host.swarms();
    let mut reaped = Vec::new();
    for session in swarms.terminal_members(swarm).await {
        if !swarms.children(swarm, session).await.is_empty() {
            // Reaping a member with live children would strand their report-back
            // edge. Reparenting happens on salvage; a finished member's children
            // are still reporting to it deliberately.
            continue;
        }
        if host.unhost(session).await.is_some() {
            reaped.push(session);
        }
    }
    reaped
}

/// The message the owner reads when work comes back.
fn salvage_report(
    name: &str,
    session: SessionId,
    tasks: &[(localpilot_taskgraph::NodeId, Salvage)],
    reparented: &[SessionId],
) -> String {
    let mut out =
        format!("Worker {name} ({session}) stopped responding and was taken off its work.");
    if tasks.is_empty() {
        out.push_str(" It held no unfinished tasks.");
    } else {
        out.push_str("\n\nIts tasks:\n");
        for (node, outcome) in tasks {
            match outcome {
                Salvage::Requeued { reclaims } => out.push_str(&format!(
                    "- {node} is back in the queue for someone else (reclaim {reclaims}).\n"
                )),
                Salvage::Exhausted { reclaims, limit } => out.push_str(&format!(
                    "- {node} has now been reclaimed {reclaims} times (limit {limit}) and has been \
                     failed. It is failing, not unlucky — look at the task itself before \
                     re-creating it.\n"
                )),
            }
        }
    }
    if !reparented.is_empty() {
        out.push_str(&format!(
            "\n{} agent(s) it had spawned now report to you.\n",
            reparented.len()
        ));
    }
    out
}

// --- durable snapshots -----------------------------------------------------

/// One swarm's durable state.
///
/// A separate stream from the session event logs on purpose: recovering a plan
/// should not require replaying anybody's transcript. This is the smallest thing
/// that lets a restarted server know what was being worked on and by whom.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwarmSnapshot {
    /// Which swarm.
    pub swarm: SwarmId,
    /// Increments on every write, so a stale writer cannot clobber a newer file.
    pub revision: u64,
    /// The membership, ordered for a stable file.
    pub members: Vec<SwarmMember>,
    /// Who was coordinating.
    pub coordinator: Option<SessionId>,
    /// The shared plan.
    pub plan: Option<TaskPlan>,
}

/// Why a snapshot could not be written or read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SnapshotError {
    /// The filesystem refused.
    #[error("{path}: {source}")]
    Io {
        /// What was being written or read.
        path: String,
        /// The underlying failure.
        source: std::io::Error,
    },
    /// The file is not a snapshot this version understands.
    #[error("{path}: {source}")]
    Malformed {
        /// The unreadable file.
        path: String,
        /// The parse failure.
        source: serde_json::Error,
    },
}

/// Durable per-swarm snapshots, primary plus backup.
///
/// Writes are serialised through a mutex and land atomically (temp then
/// rename), and the previous good file is kept as a backup — so a crash during
/// a write costs at most the newest revision, never the whole plan.
pub struct SnapshotStore {
    dir: PathBuf,
    /// Serialises writes. A snapshot is one file per swarm, and two overlapping
    /// writes to it would interleave a rename with a backup copy.
    write_lock: tokio::sync::Mutex<()>,
}

impl SnapshotStore {
    /// A store rooted at `dir`, which is created on first write.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            write_lock: tokio::sync::Mutex::new(()),
        }
    }

    fn primary(&self, swarm: &SwarmId) -> PathBuf {
        self.dir.join(format!("{}.json", file_key(swarm)))
    }

    fn backup(&self, swarm: &SwarmId) -> PathBuf {
        self.dir.join(format!("{}.json.bak", file_key(swarm)))
    }

    /// Capture a swarm's current state and write it.
    ///
    /// # Errors
    /// [`SnapshotError::Io`] if the directory or file cannot be written.
    pub async fn capture(
        &self,
        host: &SwarmHost,
        swarm: &SwarmId,
    ) -> Result<SwarmSnapshot, SnapshotError> {
        let previous = self.load(swarm).await.ok().flatten();
        let snapshot = SwarmSnapshot {
            swarm: swarm.clone(),
            revision: previous.map_or(1, |snapshot| snapshot.revision + 1),
            members: host.swarms().members(swarm).await,
            coordinator: host.swarms().coordinator(swarm).await,
            plan: host.swarms().plan(swarm).await,
        };
        self.save(&snapshot).await?;
        Ok(snapshot)
    }

    /// Write `snapshot`, refusing to go backwards.
    ///
    /// A write whose revision is not newer than what is on disk is dropped: two
    /// servers, or a slow writer racing a fast one, must not be able to restore
    /// an older plan over a newer one.
    ///
    /// # Errors
    /// [`SnapshotError::Io`] if the write fails.
    pub async fn save(&self, snapshot: &SwarmSnapshot) -> Result<bool, SnapshotError> {
        let _guard = self.write_lock.lock().await;
        std::fs::create_dir_all(&self.dir).map_err(|source| SnapshotError::Io {
            path: self.dir.display().to_string(),
            source,
        })?;
        let primary = self.primary(&snapshot.swarm);

        if let Ok(Some(existing)) = read_file(&primary) {
            if existing.revision >= snapshot.revision {
                return Ok(false);
            }
            // Keep the last good file before replacing it, so a crash mid-write
            // costs the newest revision rather than the plan.
            let _ = std::fs::copy(&primary, self.backup(&snapshot.swarm));
        }

        let body =
            serde_json::to_vec_pretty(snapshot).map_err(|source| SnapshotError::Malformed {
                path: primary.display().to_string(),
                source,
            })?;
        let temp = primary.with_extension("json.tmp");
        std::fs::write(&temp, &body).map_err(|source| SnapshotError::Io {
            path: temp.display().to_string(),
            source,
        })?;
        std::fs::rename(&temp, &primary).map_err(|source| SnapshotError::Io {
            path: primary.display().to_string(),
            source,
        })?;
        Ok(true)
    }

    /// Read a swarm's snapshot, falling back to the backup if the primary is
    /// unreadable.
    ///
    /// A torn primary is exactly what the backup is for, so a parse failure
    /// there is not an error — it is the case the backup exists to answer.
    ///
    /// # Errors
    /// [`SnapshotError::Malformed`] only when *both* files are unreadable.
    pub async fn load(&self, swarm: &SwarmId) -> Result<Option<SwarmSnapshot>, SnapshotError> {
        match read_file(&self.primary(swarm)) {
            Ok(Some(snapshot)) => Ok(Some(snapshot)),
            Ok(None) | Err(_) => match read_file(&self.backup(swarm)) {
                Ok(found) => Ok(found),
                Err(source) => Err(SnapshotError::Malformed {
                    path: self.backup(swarm).display().to_string(),
                    source,
                }),
            },
        }
    }

    /// Put a snapshot back into a live registry.
    pub async fn restore(&self, host: &SwarmHost, snapshot: SwarmSnapshot) {
        host.swarms()
            .restore(
                &snapshot.swarm,
                snapshot.members,
                snapshot.coordinator,
                snapshot.plan,
            )
            .await;
    }
}

/// Read one snapshot file. `Ok(None)` means it is not there.
fn read_file(path: &Path) -> Result<Option<SwarmSnapshot>, serde_json::Error> {
    let Ok(body) = std::fs::read(path) else {
        return Ok(None);
    };
    serde_json::from_slice(&body).map(Some)
}

/// A filesystem-safe name for a swarm id, which is otherwise opaque text that
/// may contain anything the environment override put in it.
fn file_key(swarm: &SwarmId) -> String {
    swarm
        .as_str()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_taskgraph::{NodeId, NodeSpec, PlanMode};

    /// A real node id — the type has no public constructor, deliberately: an id
    /// that no plan minted names nothing.
    fn a_node() -> NodeId {
        let lead = ActorId::new("lead");
        let mut plan = TaskPlan::new("test", PlanMode::Light, lead.clone());
        localpilot_taskgraph::ops::seed(&mut plan, &lead, "k", &[NodeSpec::task("a", "do a")])
            .expect("a one-task seed is valid")
            .nodes[0]
    }

    #[test]
    fn a_salvage_report_names_the_worker_and_what_became_of_its_work() {
        let session = SessionId::new();
        let report = salvage_report(
            "alpha",
            session,
            &[(a_node(), Salvage::Requeued { reclaims: 1 })],
            &[SessionId::new()],
        );
        assert!(report.contains("alpha"));
        assert!(report.contains("stopped responding"));
        assert!(report.contains("back in the queue"));
        assert!(report.contains("now report to you"));
    }

    #[test]
    fn an_exhausted_task_says_it_is_failing_not_unlucky() {
        let report = salvage_report(
            "alpha",
            SessionId::new(),
            &[(
                a_node(),
                Salvage::Exhausted {
                    reclaims: 2,
                    limit: 2,
                },
            )],
            &[],
        );
        assert!(report.contains("failing, not unlucky"), "{report}");
    }

    #[test]
    fn a_worker_with_no_open_tasks_is_reported_as_such() {
        let report = salvage_report("alpha", SessionId::new(), &[], &[]);
        assert!(report.contains("held no unfinished tasks"), "{report}");
    }

    #[test]
    fn a_swarm_id_becomes_a_safe_filename() {
        assert_eq!(file_key(&SwarmId::new("git-abc123")), "git-abc123");
        assert_eq!(
            file_key(&SwarmId::new("../../etc/passwd")),
            "______etc_passwd",
            "path separators and dots must not survive into a filename"
        );
    }

    #[tokio::test]
    async fn a_snapshot_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let swarm = SwarmId::new("test");
        let snapshot = SwarmSnapshot {
            swarm: swarm.clone(),
            revision: 1,
            members: vec![SwarmMember::worker(
                SessionId::new(),
                "alpha",
                SessionId::new(),
            )],
            coordinator: None,
            plan: None,
        };

        assert!(store.save(&snapshot).await.unwrap());
        assert_eq!(store.load(&swarm).await.unwrap(), Some(snapshot));
    }

    #[tokio::test]
    async fn a_stale_write_cannot_clobber_a_newer_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let swarm = SwarmId::new("test");
        let newer = SwarmSnapshot {
            swarm: swarm.clone(),
            revision: 5,
            members: Vec::new(),
            coordinator: None,
            plan: None,
        };
        let older = SwarmSnapshot {
            revision: 4,
            ..newer.clone()
        };

        assert!(store.save(&newer).await.unwrap());
        assert!(
            !store.save(&older).await.unwrap(),
            "an older revision must be dropped, not written"
        );
        assert_eq!(store.load(&swarm).await.unwrap().unwrap().revision, 5);
    }

    #[tokio::test]
    async fn a_torn_primary_falls_back_to_the_backup() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        let swarm = SwarmId::new("test");

        let first = SwarmSnapshot {
            swarm: swarm.clone(),
            revision: 1,
            members: Vec::new(),
            coordinator: None,
            plan: None,
        };
        store.save(&first).await.unwrap();
        // A second write copies the first to the backup.
        store
            .save(&SwarmSnapshot {
                revision: 2,
                ..first.clone()
            })
            .await
            .unwrap();

        // Now corrupt the primary, as a crash mid-write would.
        std::fs::write(store.primary(&swarm), b"{ not json").unwrap();
        let recovered = store.load(&swarm).await.unwrap().unwrap();
        assert_eq!(
            recovered.revision, 1,
            "the plan survives, one revision behind"
        );
    }

    #[tokio::test]
    async fn a_missing_snapshot_is_absence_not_failure() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path());
        assert_eq!(store.load(&SwarmId::new("nothing")).await.unwrap(), None);
    }
}
