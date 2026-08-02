//! Who belongs to which swarm, who spawned whom, and how many there may be.
//!
//! This sits **alongside** the [session registry](crate::registry), never inside
//! it. A session is a session whether or not it is collaborating; making the
//! session registry aware of swarms would put a multi-agent concept on the path
//! every single-agent turn already takes.
//!
//! Three things are worth stating outright, because each replaces something more
//! obvious that does not work:
//!
//! - **The spawn tree is one edge.** A member stores only who it reports back to.
//!   Children, ancestry, and subtrees are all derived by walking that edge. A
//!   stored child list would be a second copy of the same fact, and the two
//!   would disagree the first time a member departed.
//! - **Admission is a reservation, not an insertion.** Building a worker session
//!   is slow, and the cap has to be enforced *before* that work starts, under the
//!   same lock that counts it. So a spawn reserves a slot, builds, and then
//!   confirms — or releases, if the build failed. Checking the cap and inserting
//!   afterwards would let a burst of concurrent spawns all read the same count
//!   and all proceed.
//! - **An idempotency key is answered from the reservation table too.** A retried
//!   spawn whose first attempt is still building must not start a second worker;
//!   it has to be told the first one is in flight.

use std::collections::HashMap;

use localpilot_core::SessionId;
use localpilot_taskgraph::TaskPlan;
use tokio::sync::RwLock;

use super::scope::SwarmId;

/// How large a swarm may get.
///
/// These are **resource-containment** bounds — how many workers, and therefore
/// how many concurrently loaded models, a swarm may hold — **not** a token or
/// spend budget. `max_active` is the one that caps RAM/VRAM: each running worker
/// holds one model, so N agents on N models is N model loads, and this is what
/// stops a fan-out from exhausting the machine. Nothing here counts tokens;
/// provider rate-limit windows are a separate concern (`localpilot-quota`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwarmLimits {
    /// The most members one swarm may ever admit, counting departed ones.
    ///
    /// A lifetime cap rather than a live one, deliberately: the failure this
    /// guards against is a coordinator that keeps spawning replacements for work
    /// that keeps failing, and a live-only cap would let that run forever as long
    /// as the corpses were tidied up.
    pub max_members: usize,
    /// How many members may be running at once — the concurrency budget. Excess
    /// spawns are refused rather than queued here; queuing is the driver's job,
    /// where the plan that wants the work lives.
    pub max_active: usize,
}

impl Default for SwarmLimits {
    fn default() -> Self {
        Self {
            max_members: 32,
            max_active: 4,
        }
    }
}

/// What a member is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberRole {
    /// Owns the plan and may address the whole swarm.
    Coordinator,
    /// Does the work it is given.
    Worker,
}

/// Where a member is in its life.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MemberStatus {
    /// Running, or waiting for work.
    Active,
    /// Finished its assignment and reported back.
    Finished,
    /// Stopped badly.
    Failed {
        /// What went wrong.
        reason: String,
    },
    /// Gone without reporting — the case the failure lifecycle exists for.
    Departed,
}

impl MemberStatus {
    /// Whether this member still counts against the concurrency budget.
    #[must_use]
    pub fn is_active(&self) -> bool {
        matches!(self, MemberStatus::Active)
    }
}

/// One session's membership of one swarm.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SwarmMember {
    /// The session this member is.
    pub session: SessionId,
    /// A short name a peer can address it by, when it is unambiguous.
    pub name: String,
    /// Coordinator or worker.
    pub role: MemberRole,
    /// Where it is in its life.
    pub status: MemberStatus,
    /// Who it reports back to — the spawn-tree edge, and the *only* stored
    /// structure. `None` for a root.
    pub report_back_to: Option<SessionId>,
    /// What it said when it finished.
    pub completion: Option<String>,
}

impl SwarmMember {
    /// A worker spawned by `parent`.
    #[must_use]
    pub fn worker(session: SessionId, name: impl Into<String>, parent: SessionId) -> Self {
        Self {
            session,
            name: name.into(),
            role: MemberRole::Worker,
            status: MemberStatus::Active,
            report_back_to: Some(parent),
            completion: None,
        }
    }
}

/// A slot held while a worker is being built.
///
/// Not `Clone`, and it carries the swarm it belongs to, so it cannot be
/// confirmed against a different swarm than the one it was counted against.
#[derive(Debug, PartialEq, Eq)]
pub struct Reservation {
    id: u64,
    swarm: SwarmId,
}

impl Reservation {
    /// Which swarm this slot was taken in.
    #[must_use]
    pub fn swarm(&self) -> &SwarmId {
        &self.swarm
    }
}

/// What an admission request resolved to.
#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    /// A slot was taken. Build the worker, then confirm or release it.
    Reserved(Reservation),
    /// This idempotency key is already building. The caller retried; there is
    /// nothing for it to do but wait for the first attempt.
    InFlight,
    /// This idempotency key already produced a member.
    Existing(SessionId),
}

/// Why a swarm operation was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SwarmError {
    /// The swarm has admitted as many members as it ever may.
    #[error(
        "this swarm has already had {cap} members, which is its limit — finish or drop existing \
         work instead of adding more"
    )]
    MemberCapReached {
        /// The lifetime cap.
        cap: usize,
    },
    /// The concurrency budget is full.
    #[error("{cap} members are already running, which is the budget — wait for one to finish")]
    ConcurrencyReached {
        /// The budget.
        cap: usize,
    },
    /// No such swarm is registered.
    #[error("no swarm {0} is registered")]
    UnknownSwarm(SwarmId),
    /// No such member.
    #[error("session {0} is not a member of this swarm")]
    UnknownMember(SessionId),
    /// No member carries that name.
    #[error("no member of this swarm is called {0:?}")]
    UnknownName(String),
    /// Several members carry that name, so it does not identify one.
    #[error(
        "{count} members of this swarm are called {name:?} — address the one you mean by its \
         session id"
    )]
    AmbiguousName {
        /// The name that was asked for.
        name: String,
        /// How many carry it.
        count: usize,
    },
    /// The reservation is not outstanding: it was already confirmed, released,
    /// or belongs to a swarm that no longer exists.
    #[error("that spawn slot is no longer outstanding")]
    StaleReservation,
}

/// One swarm's state.
#[derive(Debug, Default)]
struct Swarm {
    members: HashMap<SessionId, SwarmMember>,
    /// Slots taken by spawns that have not finished building.
    reservations: HashMap<u64, Option<String>>,
    /// Idempotency keys that already produced a member.
    settled_keys: HashMap<String, SessionId>,
    /// How many members this swarm has ever admitted, including departed ones.
    admitted: usize,
    coordinator: Option<SessionId>,
    plan: Option<TaskPlan>,
    /// When each member was last known to be alive. Kept beside the members
    /// rather than on them: a heartbeat is an observation about a member, not a
    /// property of one, and putting an `Instant` on `SwarmMember` would make the
    /// record unserialisable for no benefit.
    heartbeats: HashMap<SessionId, std::time::Instant>,
}

impl Swarm {
    /// How many slots are currently spoken for: running members plus spawns in
    /// flight. Counting reservations is the point — a cap that ignored them
    /// would be a cap on *finished* spawns.
    fn in_use(&self) -> usize {
        self.members
            .values()
            .filter(|m| m.status.is_active())
            .count()
            + self.reservations.len()
    }
}

/// Every swarm this server hosts.
///
/// Cloning is cheap (an [`Arc`](std::sync::Arc) bump) and yields another handle
/// onto the same state.
#[derive(Clone, Default)]
pub struct SwarmRegistry {
    inner: std::sync::Arc<RwLock<Registry>>,
}

#[derive(Default)]
struct Registry {
    swarms: HashMap<SwarmId, Swarm>,
    /// Which swarm a session belongs to. A tool call knows only its own session
    /// id, so without this every lookup would be a scan of every swarm.
    by_session: HashMap<SessionId, SwarmId>,
    limits: SwarmLimits,
    next_reservation: u64,
}

impl SwarmRegistry {
    /// An empty registry with default limits.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty registry with the given limits.
    #[must_use]
    pub fn with_limits(limits: SwarmLimits) -> Self {
        Self {
            inner: std::sync::Arc::new(RwLock::new(Registry {
                limits,
                ..Registry::default()
            })),
        }
    }

    /// The configured limits.
    pub async fn limits(&self) -> SwarmLimits {
        self.inner.read().await.limits
    }

    /// Register a root member — a session that was not spawned by anyone.
    ///
    /// The first root of a swarm becomes its coordinator. A root still counts
    /// against the caps: a swarm of nothing but roots is still a swarm.
    ///
    /// # Errors
    /// [`SwarmError::MemberCapReached`] or [`SwarmError::ConcurrencyReached`].
    pub async fn join_as_root(
        &self,
        swarm: &SwarmId,
        session: SessionId,
        name: impl Into<String>,
    ) -> Result<MemberRole, SwarmError> {
        let mut guard = self.inner.write().await;
        let limits = guard.limits;
        let entry = guard.swarms.entry(swarm.clone()).or_default();
        // An already-registered session is a re-join, not a new member: it must
        // not consume a second slot.
        if let Some(existing) = entry.members.get(&session) {
            return Ok(existing.role);
        }
        check_caps(entry, limits)?;
        let role = if entry.coordinator.is_none() {
            entry.coordinator = Some(session);
            MemberRole::Coordinator
        } else {
            MemberRole::Worker
        };
        entry.admitted += 1;
        entry.members.insert(
            session,
            SwarmMember {
                session,
                name: name.into(),
                role,
                status: MemberStatus::Active,
                report_back_to: None,
                completion: None,
            },
        );
        guard.by_session.insert(session, swarm.clone());
        Ok(role)
    }

    /// Take a slot for a spawn that is about to start.
    ///
    /// The caps are checked and the slot is taken under one write lock, so
    /// concurrent spawns cannot all read the same count and all proceed. Confirm
    /// with [`confirm`](Self::confirm) once the worker exists, or
    /// [`release`](Self::release) if building it failed — a reservation that is
    /// neither holds a slot for the life of the server.
    ///
    /// # Errors
    /// [`SwarmError::MemberCapReached`] or [`SwarmError::ConcurrencyReached`].
    pub async fn reserve(
        &self,
        swarm: &SwarmId,
        idempotency_key: Option<&str>,
    ) -> Result<Admission, SwarmError> {
        let mut guard = self.inner.write().await;
        let limits = guard.limits;
        let id = guard.next_reservation;
        let entry = guard.swarms.entry(swarm.clone()).or_default();

        if let Some(key) = idempotency_key {
            if let Some(session) = entry.settled_keys.get(key) {
                return Ok(Admission::Existing(*session));
            }
            if entry
                .reservations
                .values()
                .any(|held| held.as_deref() == Some(key))
            {
                return Ok(Admission::InFlight);
            }
        }
        check_caps(entry, limits)?;
        entry
            .reservations
            .insert(id, idempotency_key.map(ToOwned::to_owned));
        guard.next_reservation += 1;
        Ok(Admission::Reserved(Reservation {
            id,
            swarm: swarm.clone(),
        }))
    }

    /// Turn a held slot into a real member.
    ///
    /// # Errors
    /// [`SwarmError::StaleReservation`] if the slot is no longer outstanding.
    pub async fn confirm(
        &self,
        reservation: Reservation,
        member: SwarmMember,
    ) -> Result<(), SwarmError> {
        let mut guard = self.inner.write().await;
        let swarm = reservation.swarm.clone();
        let entry = guard
            .swarms
            .get_mut(&swarm)
            .ok_or(SwarmError::StaleReservation)?;
        let key = entry
            .reservations
            .remove(&reservation.id)
            .ok_or(SwarmError::StaleReservation)?;
        if let Some(key) = key {
            entry.settled_keys.insert(key, member.session);
        }
        entry.admitted += 1;
        let session = member.session;
        entry.members.insert(session, member);
        guard.by_session.insert(session, swarm);
        Ok(())
    }

    /// Give a held slot back, because the worker was never built.
    ///
    /// Returns whether the slot was still outstanding. A release of an already
    /// released slot is not an error — the caller is on a failure path, and
    /// making it handle a second failure there helps nobody.
    pub async fn release(&self, reservation: Reservation) -> bool {
        let mut guard = self.inner.write().await;
        guard
            .swarms
            .get_mut(&reservation.swarm)
            .and_then(|entry| entry.reservations.remove(&reservation.id))
            .is_some()
    }

    /// Which swarm a session belongs to.
    pub async fn swarm_of(&self, session: SessionId) -> Option<SwarmId> {
        self.inner.read().await.by_session.get(&session).cloned()
    }

    /// One member.
    pub async fn member(&self, swarm: &SwarmId, session: SessionId) -> Option<SwarmMember> {
        self.inner
            .read()
            .await
            .swarms
            .get(swarm)
            .and_then(|entry| entry.members.get(&session))
            .cloned()
    }

    /// Every member, ordered by session id so callers and tests see a stable
    /// list rather than whatever the hash map felt like.
    pub async fn members(&self, swarm: &SwarmId) -> Vec<SwarmMember> {
        let guard = self.inner.read().await;
        let Some(entry) = guard.swarms.get(swarm) else {
            return Vec::new();
        };
        let mut out: Vec<SwarmMember> = entry.members.values().cloned().collect();
        out.sort_by_key(|m| m.session.as_uuid());
        out
    }

    /// The swarm's coordinator.
    pub async fn coordinator(&self, swarm: &SwarmId) -> Option<SessionId> {
        self.inner
            .read()
            .await
            .swarms
            .get(swarm)
            .and_then(|entry| entry.coordinator)
    }

    /// Whether `session` is the coordinator of its swarm.
    pub async fn is_coordinator(&self, swarm: &SwarmId, session: SessionId) -> bool {
        self.coordinator(swarm).await == Some(session)
    }

    /// Record that a member is alive now.
    ///
    /// Called whenever a member does anything observable. A heartbeat is cheap
    /// and a missed one is expensive, so this is deliberately not gated on
    /// anything.
    pub async fn heartbeat(&self, swarm: &SwarmId, session: SessionId) {
        self.heartbeat_at(swarm, session, std::time::Instant::now())
            .await;
    }

    /// [`heartbeat`](Self::heartbeat) at an explicit instant.
    ///
    /// Time is a parameter for the same reason it is on the touch index: a test
    /// for "this member went quiet ninety seconds ago" cannot wait ninety
    /// seconds, and one that reads the clock itself can only be written to pass.
    pub async fn heartbeat_at(&self, swarm: &SwarmId, session: SessionId, at: std::time::Instant) {
        let mut guard = self.inner.write().await;
        if let Some(entry) = guard.swarms.get_mut(swarm) {
            entry.heartbeats.insert(session, at);
        }
    }

    /// Active members that have not been heard from within `within`, in id
    /// order.
    ///
    /// A member with no heartbeat at all is *not* stale: it has never had the
    /// chance to beat. Treating silence-since-birth as death would reap every
    /// worker the instant it was admitted.
    pub async fn stale_members(
        &self,
        swarm: &SwarmId,
        within: std::time::Duration,
        now: std::time::Instant,
    ) -> Vec<SessionId> {
        let guard = self.inner.read().await;
        let Some(entry) = guard.swarms.get(swarm) else {
            return Vec::new();
        };
        let mut out: Vec<SessionId> = entry
            .members
            .values()
            .filter(|member| member.status.is_active())
            .filter(|member| {
                entry
                    .heartbeats
                    .get(&member.session)
                    .is_some_and(|last| now.duration_since(*last) > within)
            })
            .map(|member| member.session)
            .collect();
        out.sort_by_key(SessionId::as_uuid);
        out
    }

    /// Elect a coordinator, if the seat is empty and anyone is left.
    ///
    /// The successor is the lowest active member id. Deterministic on purpose:
    /// every observer of the same state elects the same member, so there is no
    /// window in which two of them believe different things.
    ///
    /// Returns the new coordinator, or `None` if the seat was filled or there is
    /// nobody to fill it.
    pub async fn elect_coordinator(&self, swarm: &SwarmId) -> Option<SessionId> {
        let mut guard = self.inner.write().await;
        let entry = guard.swarms.get_mut(swarm)?;
        if entry
            .coordinator
            .is_some_and(|id| entry.members.get(&id).is_some_and(|m| m.status.is_active()))
        {
            return None;
        }
        let successor = entry
            .members
            .values()
            .filter(|member| member.status.is_active())
            .map(|member| member.session)
            .min_by_key(SessionId::as_uuid)?;
        entry.coordinator = Some(successor);
        if let Some(member) = entry.members.get_mut(&successor) {
            member.role = MemberRole::Coordinator;
        }
        Some(successor)
    }

    /// Hand a departed member's children to the nearest surviving ancestor.
    ///
    /// Grandparent first, then the coordinator, then nothing — a child whose
    /// whole line is gone becomes a root rather than pointing at a member that
    /// no longer exists, because a dangling report-back edge is a completion
    /// report delivered nowhere.
    ///
    /// Returns the children that were moved.
    pub async fn reparent_children(&self, swarm: &SwarmId, departed: SessionId) -> Vec<SessionId> {
        let mut guard = self.inner.write().await;
        let Some(entry) = guard.swarms.get_mut(swarm) else {
            return Vec::new();
        };
        let grandparent = entry
            .members
            .get(&departed)
            .and_then(|member| member.report_back_to)
            .filter(|id| {
                entry
                    .members
                    .get(id)
                    .is_some_and(|m| m.status.is_active() && m.session != departed)
            });
        let new_parent = grandparent.or_else(|| {
            entry
                .coordinator
                .filter(|id| *id != departed && entry.members.contains_key(id))
        });

        let moved: Vec<SessionId> = entry
            .members
            .values()
            .filter(|member| member.report_back_to == Some(departed))
            .map(|member| member.session)
            .collect();
        for id in &moved {
            if let Some(member) = entry.members.get_mut(id) {
                // Never reparent a member onto itself: that is a cycle the tree
                // walks would then have to defend against forever.
                member.report_back_to = new_parent.filter(|parent| parent != id);
            }
        }
        let mut moved = moved;
        moved.sort_by_key(SessionId::as_uuid);
        moved
    }

    /// Every member in a terminal state, in id order — the reaper's candidates.
    pub async fn terminal_members(&self, swarm: &SwarmId) -> Vec<SessionId> {
        let guard = self.inner.read().await;
        let Some(entry) = guard.swarms.get(swarm) else {
            return Vec::new();
        };
        let mut out: Vec<SessionId> = entry
            .members
            .values()
            .filter(|member| !member.status.is_active())
            .map(|member| member.session)
            .collect();
        out.sort_by_key(SessionId::as_uuid);
        out
    }

    /// Replace this swarm's members and plan wholesale — the restore path.
    pub async fn restore(
        &self,
        swarm: &SwarmId,
        members: Vec<SwarmMember>,
        coordinator: Option<SessionId>,
        plan: Option<TaskPlan>,
    ) {
        let mut guard = self.inner.write().await;
        let admitted = members.len();
        let mut map = HashMap::new();
        for member in members {
            guard.by_session.insert(member.session, swarm.clone());
            map.insert(member.session, member);
        }
        let entry = guard.swarms.entry(swarm.clone()).or_default();
        entry.members = map;
        entry.coordinator = coordinator;
        entry.plan = plan;
        // A restored swarm has already spent those admissions; resetting the
        // count would hand a recovered coordinator a fresh budget to burn.
        entry.admitted = entry.admitted.max(admitted);
    }

    /// Move a member to a new status.
    ///
    /// # Errors
    /// [`SwarmError::UnknownSwarm`] or [`SwarmError::UnknownMember`].
    pub async fn set_status(
        &self,
        swarm: &SwarmId,
        session: SessionId,
        status: MemberStatus,
    ) -> Result<(), SwarmError> {
        let mut guard = self.inner.write().await;
        let entry = guard
            .swarms
            .get_mut(swarm)
            .ok_or_else(|| SwarmError::UnknownSwarm(swarm.clone()))?;
        let member = entry
            .members
            .get_mut(&session)
            .ok_or(SwarmError::UnknownMember(session))?;
        member.status = status;
        Ok(())
    }

    /// Record what a member said when it finished, and mark it finished.
    ///
    /// # Errors
    /// [`SwarmError::UnknownSwarm`] or [`SwarmError::UnknownMember`].
    pub async fn record_completion(
        &self,
        swarm: &SwarmId,
        session: SessionId,
        report: impl Into<String>,
    ) -> Result<Option<SessionId>, SwarmError> {
        let mut guard = self.inner.write().await;
        let entry = guard
            .swarms
            .get_mut(swarm)
            .ok_or_else(|| SwarmError::UnknownSwarm(swarm.clone()))?;
        let member = entry
            .members
            .get_mut(&session)
            .ok_or(SwarmError::UnknownMember(session))?;
        member.completion = Some(report.into());
        member.status = MemberStatus::Finished;
        Ok(member.report_back_to)
    }

    /// Resolve a friendly name to exactly one member.
    ///
    /// Ambiguity is an error rather than a first-match, because "send this to
    /// `reviewer`" reaching an arbitrary one of two reviewers is worse than not
    /// being sent.
    ///
    /// # Errors
    /// [`SwarmError::UnknownName`] or [`SwarmError::AmbiguousName`].
    pub async fn resolve_name(&self, swarm: &SwarmId, name: &str) -> Result<SessionId, SwarmError> {
        let guard = self.inner.read().await;
        let Some(entry) = guard.swarms.get(swarm) else {
            return Err(SwarmError::UnknownName(name.to_string()));
        };
        let mut matches: Vec<SessionId> = entry
            .members
            .values()
            .filter(|m| m.name.eq_ignore_ascii_case(name))
            .map(|m| m.session)
            .collect();
        match matches.len() {
            0 => Err(SwarmError::UnknownName(name.to_string())),
            1 => Ok(matches.remove(0)),
            count => Err(SwarmError::AmbiguousName {
                name: name.to_string(),
                count,
            }),
        }
    }

    /// The members that report directly to `session`, in id order.
    pub async fn children(&self, swarm: &SwarmId, session: SessionId) -> Vec<SessionId> {
        let guard = self.inner.read().await;
        let Some(entry) = guard.swarms.get(swarm) else {
            return Vec::new();
        };
        let mut out: Vec<SessionId> = entry
            .members
            .values()
            .filter(|m| m.report_back_to == Some(session))
            .map(|m| m.session)
            .collect();
        out.sort_by_key(SessionId::as_uuid);
        out
    }

    /// Who `session` reports to, then who *they* report to, and so on up to the
    /// root — nearest first.
    ///
    /// Bounded by the member count, so a cycle written by a bug walks each
    /// member at most once instead of hanging the caller.
    pub async fn ancestors(&self, swarm: &SwarmId, session: SessionId) -> Vec<SessionId> {
        let guard = self.inner.read().await;
        let Some(entry) = guard.swarms.get(swarm) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut current = entry.members.get(&session).and_then(|m| m.report_back_to);
        while let Some(id) = current {
            if !seen.insert(id) {
                break;
            }
            out.push(id);
            current = entry.members.get(&id).and_then(|m| m.report_back_to);
        }
        out
    }

    /// `root` and everything beneath it in the spawn tree, in id order.
    ///
    /// The scope a broadcast uses: a member may address what it spawned, and no
    /// more. Whole-swarm reach is the coordinator's alone.
    pub async fn subtree(&self, swarm: &SwarmId, root: SessionId) -> Vec<SessionId> {
        let guard = self.inner.read().await;
        let Some(entry) = guard.swarms.get(swarm) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            if entry.members.contains_key(&id) {
                out.push(id);
            }
            stack.extend(
                entry
                    .members
                    .values()
                    .filter(|m| m.report_back_to == Some(id))
                    .map(|m| m.session),
            );
        }
        out.sort_by_key(SessionId::as_uuid);
        out
    }

    /// Store a swarm's plan, replacing any earlier one.
    pub async fn set_plan(&self, swarm: &SwarmId, plan: TaskPlan) {
        let mut guard = self.inner.write().await;
        guard.swarms.entry(swarm.clone()).or_default().plan = Some(plan);
    }

    /// A copy of a swarm's plan.
    pub async fn plan(&self, swarm: &SwarmId) -> Option<TaskPlan> {
        self.inner
            .read()
            .await
            .swarms
            .get(swarm)
            .and_then(|entry| entry.plan.clone())
    }

    /// Read, mutate, and store a swarm's plan under one write lock, returning
    /// whatever `mutate` produced.
    ///
    /// Every plan mutation goes through here rather than through
    /// `plan()` → change → `set_plan()`, which would be a read-modify-write with
    /// a gap in the middle — precisely the race a shared plan cannot afford.
    ///
    /// # Errors
    /// [`SwarmError::UnknownSwarm`] if the swarm holds no plan.
    pub async fn with_plan<T>(
        &self,
        swarm: &SwarmId,
        mutate: impl FnOnce(&mut TaskPlan) -> T,
    ) -> Result<T, SwarmError> {
        let mut guard = self.inner.write().await;
        let plan = guard
            .swarms
            .get_mut(swarm)
            .and_then(|entry| entry.plan.as_mut())
            .ok_or_else(|| SwarmError::UnknownSwarm(swarm.clone()))?;
        Ok(mutate(plan))
    }

    /// Remove a member and forget its session mapping. Returns what was removed.
    ///
    /// The lifetime admission count is *not* decremented: the cap is on how much
    /// a swarm has spawned, not on how much it is holding.
    pub async fn remove(&self, swarm: &SwarmId, session: SessionId) -> Option<SwarmMember> {
        let mut guard = self.inner.write().await;
        let removed = guard
            .swarms
            .get_mut(swarm)
            .and_then(|entry| entry.members.remove(&session));
        if removed.is_some() {
            guard.by_session.remove(&session);
            if let Some(entry) = guard.swarms.get_mut(swarm) {
                if entry.coordinator == Some(session) {
                    entry.coordinator = None;
                }
            }
        }
        removed
    }

    /// Every swarm currently holding state, in id order.
    pub async fn swarms(&self) -> Vec<SwarmId> {
        let guard = self.inner.read().await;
        let mut out: Vec<SwarmId> = guard.swarms.keys().cloned().collect();
        out.sort();
        out
    }
}

/// Both caps, checked together so a caller cannot pass one and be surprised by
/// the other after doing the expensive part.
fn check_caps(entry: &Swarm, limits: SwarmLimits) -> Result<(), SwarmError> {
    if entry.admitted + entry.reservations.len() >= limits.max_members {
        return Err(SwarmError::MemberCapReached {
            cap: limits.max_members,
        });
    }
    if entry.in_use() >= limits.max_active {
        return Err(SwarmError::ConcurrencyReached {
            cap: limits.max_active,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn swarm() -> SwarmId {
        SwarmId::new("test-swarm")
    }

    /// Reserve, then immediately confirm — the common path, compressed.
    async fn spawn(
        registry: &SwarmRegistry,
        swarm: &SwarmId,
        name: &str,
        parent: SessionId,
    ) -> SessionId {
        let Admission::Reserved(slot) = registry.reserve(swarm, None).await.unwrap() else {
            panic!("a fresh spawn reserves");
        };
        let session = SessionId::new();
        registry
            .confirm(slot, SwarmMember::worker(session, name, parent))
            .await
            .unwrap();
        session
    }

    #[tokio::test]
    async fn the_first_root_becomes_the_coordinator_and_the_second_does_not() {
        let registry = SwarmRegistry::new();
        let first = SessionId::new();
        let second = SessionId::new();

        assert_eq!(
            registry
                .join_as_root(&swarm(), first, "lead")
                .await
                .unwrap(),
            MemberRole::Coordinator
        );
        assert_eq!(
            registry
                .join_as_root(&swarm(), second, "also-here")
                .await
                .unwrap(),
            MemberRole::Worker
        );
        assert_eq!(registry.coordinator(&swarm()).await, Some(first));
        assert!(registry.is_coordinator(&swarm(), first).await);
        assert!(!registry.is_coordinator(&swarm(), second).await);
    }

    #[tokio::test]
    async fn re_joining_does_not_consume_a_second_slot() {
        let registry = SwarmRegistry::with_limits(SwarmLimits {
            max_members: 2,
            max_active: 2,
        });
        let root = SessionId::new();
        registry.join_as_root(&swarm(), root, "lead").await.unwrap();
        registry.join_as_root(&swarm(), root, "lead").await.unwrap();
        assert_eq!(registry.members(&swarm()).await.len(), 1);
        // The second slot is still free.
        assert!(matches!(
            registry.reserve(&swarm(), None).await,
            Ok(Admission::Reserved(_))
        ));
    }

    #[tokio::test]
    async fn a_session_can_be_traced_back_to_its_swarm() {
        let registry = SwarmRegistry::new();
        let root = SessionId::new();
        registry.join_as_root(&swarm(), root, "lead").await.unwrap();
        let child = spawn(&registry, &swarm(), "w", root).await;

        assert_eq!(registry.swarm_of(root).await, Some(swarm()));
        assert_eq!(registry.swarm_of(child).await, Some(swarm()));
        assert_eq!(registry.swarm_of(SessionId::new()).await, None);
    }

    #[tokio::test]
    async fn the_spawn_tree_is_derived_from_one_edge() {
        let registry = SwarmRegistry::new();
        let root = SessionId::new();
        registry.join_as_root(&swarm(), root, "lead").await.unwrap();
        let a = spawn(&registry, &swarm(), "a", root).await;
        let b = spawn(&registry, &swarm(), "b", root).await;
        let a1 = spawn(&registry, &swarm(), "a1", a).await;

        assert_eq!(registry.children(&swarm(), root).await, sorted(vec![a, b]));
        assert_eq!(registry.children(&swarm(), a).await, vec![a1]);
        assert!(registry.children(&swarm(), b).await.is_empty());

        assert_eq!(registry.ancestors(&swarm(), a1).await, vec![a, root]);
        assert_eq!(registry.ancestors(&swarm(), root).await, Vec::new());

        assert_eq!(
            registry.subtree(&swarm(), a).await,
            sorted(vec![a, a1]),
            "a subtree includes its own root"
        );
        assert_eq!(
            registry.subtree(&swarm(), root).await,
            sorted(vec![root, a, b, a1])
        );
    }

    fn sorted(mut ids: Vec<SessionId>) -> Vec<SessionId> {
        ids.sort_by_key(SessionId::as_uuid);
        ids
    }

    #[tokio::test]
    async fn concurrent_spawns_cannot_burst_past_the_budget() {
        let registry = SwarmRegistry::with_limits(SwarmLimits {
            max_members: 64,
            max_active: 3,
        });
        let attempts: Vec<_> = (0..32)
            .map(|_| {
                let registry = registry.clone();
                let swarm = swarm();
                tokio::spawn(async move { registry.reserve(&swarm, None).await })
            })
            .collect();

        let mut reserved = 0;
        let mut refused = 0;
        for attempt in attempts {
            match attempt.await.unwrap() {
                Ok(Admission::Reserved(_)) => reserved += 1,
                Err(SwarmError::ConcurrencyReached { cap: 3 }) => refused += 1,
                other => panic!("unexpected outcome: {other:?}"),
            }
        }
        assert_eq!(reserved, 3, "the budget held under a burst");
        assert_eq!(refused, 29);
    }

    #[tokio::test]
    async fn a_retried_spawn_is_deduplicated_while_in_flight_and_after() {
        let registry = SwarmRegistry::new();
        let Admission::Reserved(slot) = registry.reserve(&swarm(), Some("k1")).await.unwrap()
        else {
            panic!("first attempt reserves");
        };

        // Retried while the first is still building.
        assert_eq!(
            registry.reserve(&swarm(), Some("k1")).await.unwrap(),
            Admission::InFlight
        );

        let session = SessionId::new();
        registry
            .confirm(slot, SwarmMember::worker(session, "w", SessionId::new()))
            .await
            .unwrap();

        // Retried after it landed.
        assert_eq!(
            registry.reserve(&swarm(), Some("k1")).await.unwrap(),
            Admission::Existing(session)
        );
        assert_eq!(registry.members(&swarm()).await.len(), 1);
    }

    #[tokio::test]
    async fn a_different_key_is_a_different_spawn() {
        let registry = SwarmRegistry::new();
        let first = registry.reserve(&swarm(), Some("k1")).await.unwrap();
        let second = registry.reserve(&swarm(), Some("k2")).await.unwrap();
        assert!(matches!(first, Admission::Reserved(_)));
        assert!(matches!(second, Admission::Reserved(_)));
    }

    #[tokio::test]
    async fn a_released_slot_goes_back_to_the_budget() {
        let registry = SwarmRegistry::with_limits(SwarmLimits {
            max_members: 8,
            max_active: 1,
        });
        let Admission::Reserved(slot) = registry.reserve(&swarm(), None).await.unwrap() else {
            panic!("reserved");
        };
        assert!(matches!(
            registry.reserve(&swarm(), None).await,
            Err(SwarmError::ConcurrencyReached { .. })
        ));

        assert!(registry.release(slot).await, "the slot was outstanding");
        assert!(matches!(
            registry.reserve(&swarm(), None).await,
            Ok(Admission::Reserved(_))
        ));
    }

    #[tokio::test]
    async fn a_reservation_cannot_be_confirmed_twice() {
        let registry = SwarmRegistry::new();
        let Admission::Reserved(slot) = registry.reserve(&swarm(), None).await.unwrap() else {
            panic!("reserved");
        };
        let id = slot.id;
        let swarm_id = slot.swarm.clone();
        registry
            .confirm(
                slot,
                SwarmMember::worker(SessionId::new(), "w", SessionId::new()),
            )
            .await
            .unwrap();

        let forged = Reservation {
            id,
            swarm: swarm_id,
        };
        assert_eq!(
            registry
                .confirm(
                    forged,
                    SwarmMember::worker(SessionId::new(), "w2", SessionId::new())
                )
                .await,
            Err(SwarmError::StaleReservation)
        );
        assert_eq!(registry.members(&swarm()).await.len(), 1);
    }

    #[tokio::test]
    async fn the_lifetime_cap_counts_departed_members_too() {
        let registry = SwarmRegistry::with_limits(SwarmLimits {
            max_members: 2,
            max_active: 4,
        });
        let root = SessionId::new();
        registry.join_as_root(&swarm(), root, "lead").await.unwrap();
        let worker = spawn(&registry, &swarm(), "w", root).await;

        // Both slots are used, so the cap refuses even though tidying up would
        // free a live slot: a coordinator that keeps replacing failed work must
        // still be stopped.
        registry
            .set_status(&swarm(), worker, MemberStatus::Departed)
            .await
            .unwrap();
        assert!(matches!(
            registry.reserve(&swarm(), None).await,
            Err(SwarmError::MemberCapReached { cap: 2 })
        ));
    }

    #[tokio::test]
    async fn a_finished_member_frees_a_concurrency_slot() {
        let registry = SwarmRegistry::with_limits(SwarmLimits {
            max_members: 8,
            max_active: 1,
        });
        let root = SessionId::new();
        registry.join_as_root(&swarm(), root, "lead").await.unwrap();
        assert!(matches!(
            registry.reserve(&swarm(), None).await,
            Err(SwarmError::ConcurrencyReached { .. })
        ));

        registry
            .set_status(&swarm(), root, MemberStatus::Finished)
            .await
            .unwrap();
        assert!(matches!(
            registry.reserve(&swarm(), None).await,
            Ok(Admission::Reserved(_))
        ));
    }

    #[tokio::test]
    async fn a_completion_reports_back_to_the_spawner() {
        let registry = SwarmRegistry::new();
        let root = SessionId::new();
        registry.join_as_root(&swarm(), root, "lead").await.unwrap();
        let worker = spawn(&registry, &swarm(), "w", root).await;

        let owner = registry
            .record_completion(&swarm(), worker, "found the bug in parse.rs")
            .await
            .unwrap();
        assert_eq!(owner, Some(root));
        let member = registry.member(&swarm(), worker).await.unwrap();
        assert_eq!(member.status, MemberStatus::Finished);
        assert_eq!(
            member.completion.as_deref(),
            Some("found the bug in parse.rs")
        );
    }

    #[tokio::test]
    async fn a_name_resolves_only_when_it_is_unambiguous() {
        let registry = SwarmRegistry::new();
        let root = SessionId::new();
        registry.join_as_root(&swarm(), root, "lead").await.unwrap();
        let reviewer = spawn(&registry, &swarm(), "reviewer", root).await;

        assert_eq!(
            registry.resolve_name(&swarm(), "Reviewer").await.unwrap(),
            reviewer,
            "names are matched case-insensitively"
        );
        assert_eq!(
            registry.resolve_name(&swarm(), "nobody").await,
            Err(SwarmError::UnknownName("nobody".into()))
        );

        spawn(&registry, &swarm(), "reviewer", root).await;
        assert_eq!(
            registry.resolve_name(&swarm(), "reviewer").await,
            Err(SwarmError::AmbiguousName {
                name: "reviewer".into(),
                count: 2
            })
        );
    }

    #[tokio::test]
    async fn removing_the_coordinator_leaves_the_seat_empty() {
        let registry = SwarmRegistry::new();
        let root = SessionId::new();
        registry.join_as_root(&swarm(), root, "lead").await.unwrap();
        assert_eq!(registry.coordinator(&swarm()).await, Some(root));

        let removed = registry.remove(&swarm(), root).await.unwrap();
        assert_eq!(removed.session, root);
        assert_eq!(registry.coordinator(&swarm()).await, None);
        assert_eq!(registry.swarm_of(root).await, None);
    }

    #[tokio::test]
    async fn a_plan_is_mutated_under_one_lock() {
        use localpilot_taskgraph::ops::seed;
        use localpilot_taskgraph::{ActorId, NodeSpec, PlanMode, TaskPlan};

        let registry = SwarmRegistry::new();
        let lead = ActorId::new("lead");
        registry
            .set_plan(
                &swarm(),
                TaskPlan::new("ship it", PlanMode::Light, lead.clone()),
            )
            .await;

        let seeded = registry
            .with_plan(&swarm(), |plan| {
                seed(plan, &lead, "k1", &[NodeSpec::task("a", "do a")])
            })
            .await
            .unwrap()
            .unwrap();

        assert_eq!(seeded.nodes.len(), 1);
        assert_eq!(registry.plan(&swarm()).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn mutating_a_plan_that_does_not_exist_says_so() {
        let registry = SwarmRegistry::new();
        assert_eq!(
            registry.with_plan(&swarm(), |plan| plan.len()).await,
            Err(SwarmError::UnknownSwarm(swarm()))
        );
    }
}
