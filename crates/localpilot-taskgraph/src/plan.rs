//! The graph itself: tasks, the edges between them, and the plan that owns both.
//!
//! Two properties are structural rather than checked-at-use:
//!
//! - **Determinism.** Nodes live in a [`BTreeMap`] keyed by a monotonically
//!   minted [`NodeId`], so every traversal — the ready set, the dispatch order,
//!   a rendered input — is in the same order on every machine and every run. A
//!   `HashMap` here would make the simulator's output depend on the allocator,
//!   which would make it useless as a safety net.
//! - **Edge direction.** A node stores only what it *waits on*. Dependents are
//!   derived by scanning, which is O(n) on a plan of tens of nodes and removes
//!   the entire class of bug where the two directions disagree.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::artifact::HandoffArtifact;

/// Who is acting on the plan.
///
/// Deliberately a plain string rather than a session type: this crate is a leaf
/// with no LocalPilot dependencies, so it can be simulated and property-tested
/// without a runtime. The server maps its own session ids into this.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ActorId(String);

impl ActorId {
    /// Wrap an identifier.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The identifier as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ActorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for ActorId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for ActorId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// A task's identity within one plan. Minted in creation order, so the number
/// itself is a readable trace of how the plan grew.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(u32);

impl NodeId {
    /// The underlying number, for display and for stable test assertions.
    #[must_use]
    pub fn get(self) -> u32 {
        self.0
    }

    /// Forge an id that was never minted, to stand in for a corrupted snapshot.
    /// Test-only: outside a test there is no legitimate way to name a node the
    /// plan did not create, and offering one would defeat the id's whole point.
    #[cfg(test)]
    pub(crate) fn forged(raw: u32) -> Self {
        Self(raw)
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// What a node is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    /// Ordinary work.
    Task,
    /// A review of everything it waits on. A gate may inject follow-up work
    /// instead of passing, which is the only way new work enters a plan after
    /// its author has stopped looking at it.
    Gate,
}

/// Where a node is in its life.
///
/// `Ready` is deliberately absent: readiness is a *function of the graph*
/// (pending, and every dependency complete), so storing it would create a second
/// source of truth that could disagree with the edges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TaskStatus {
    /// Waiting — either on a dependency, or on a worker.
    Pending,
    /// Dispatched to an actor and in flight.
    Assigned {
        /// Who is working on it.
        assignee: ActorId,
    },
    /// Finished, with a handoff for everything downstream.
    Complete,
    /// Finished badly. Terminal: a failed task is requeued by creating new work,
    /// never by reviving the record of what went wrong.
    Failed {
        /// Why, in the words of whoever reported it.
        reason: String,
    },
    /// Dropped deliberately — the plan changed under it.
    Abandoned {
        /// Why it was dropped.
        reason: String,
    },
}

impl TaskStatus {
    /// Whether this state can still change.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskStatus::Complete | TaskStatus::Failed { .. } | TaskStatus::Abandoned { .. }
        )
    }

    /// A short label for error messages.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::Assigned { .. } => "assigned",
            TaskStatus::Complete => "complete",
            TaskStatus::Failed { .. } => "failed",
            TaskStatus::Abandoned { .. } => "abandoned",
        }
    }
}

/// One unit of work in the graph.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskNode {
    /// Identity within the plan.
    pub id: NodeId,
    /// A short human-readable name.
    pub title: String,
    /// The instruction handed to whoever takes this on.
    pub prompt: String,
    /// Task or review gate.
    pub kind: NodeKind,
    /// Who may mutate this node. Ownership is what stops one worker rewriting
    /// another's subtree.
    pub owner: ActorId,
    /// Where it is in its life.
    pub status: TaskStatus,
    /// What it waits on. Sorted and deduplicated on write.
    pub upstream: Vec<NodeId>,
    /// The handoff, once complete.
    pub artifact: Option<HandoffArtifact>,
    /// How many times this task has been salvaged from a departed assignee. A
    /// task that keeps coming back is failing, not unlucky, so the count is a
    /// budget rather than a statistic.
    #[serde(default)]
    pub reclaims: u32,
}

impl TaskNode {
    /// Whether this node can no longer change.
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    /// Who holds the assignment, if anyone.
    #[must_use]
    pub fn assignee(&self) -> Option<&ActorId> {
        match &self.status {
            TaskStatus::Assigned { assignee } => Some(assignee),
            _ => None,
        }
    }
}

/// How strictly a plan is run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanMode {
    /// Decomposition and ordering, nothing more. A completion may be a sentence.
    #[default]
    Light,
    /// Decomposition plus accountability: every seed and every expansion is
    /// gated, and a completion must carry findings *and* an explicit statement
    /// of what it did not check.
    Deep,
}

impl PlanMode {
    /// Whether this mode inserts review gates and demands full handoffs.
    #[must_use]
    pub fn is_deep(self) -> bool {
        matches!(self, PlanMode::Deep)
    }
}

/// A description of a node to create. Dependencies are given as positions in the
/// same batch, so a caller can describe a whole sub-graph in one call without
/// having to know the ids it has not been given yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeSpec {
    /// A short human-readable name.
    pub title: String,
    /// The instruction handed to whoever takes this on.
    pub prompt: String,
    /// Task or review gate. Defaults to an ordinary task.
    #[serde(default = "default_kind")]
    pub kind: NodeKind,
    /// Positions *within this batch* that this node waits on.
    #[serde(default)]
    pub depends_on_batch: Vec<usize>,
}

fn default_kind() -> NodeKind {
    NodeKind::Task
}

impl NodeSpec {
    /// An ordinary task with no in-batch dependencies.
    #[must_use]
    pub fn task(title: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            prompt: prompt.into(),
            kind: NodeKind::Task,
            depends_on_batch: Vec::new(),
        }
    }

    /// A review gate.
    #[must_use]
    pub fn gate(title: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            kind: NodeKind::Gate,
            ..Self::task(title, prompt)
        }
    }

    /// Wait on another entry in the same batch.
    #[must_use]
    pub fn after(mut self, index: usize) -> Self {
        self.depends_on_batch.push(index);
        self
    }
}

/// A task graph and everything needed to replay it.
///
/// The plan carries a `version` that increments on every accepted mutation. It
/// is not decoration: a snapshot restored after a crash, and a client deciding
/// whether its view is stale, both need a single number that says "the graph
/// changed" without diffing it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskPlan {
    /// What the plan is for, in a line. Travels into every assignment so a
    /// worker knows what it is contributing to.
    objective: String,
    /// How strictly it is run.
    mode: PlanMode,
    /// Whoever seeded it. The default owner of auto-inserted gates.
    coordinator: ActorId,
    /// Incremented on every accepted mutation.
    version: u64,
    /// The graph, ordered by id so every traversal is deterministic.
    nodes: BTreeMap<NodeId, TaskNode>,
    /// The next id to mint.
    next_id: u32,
    /// Seed keys already applied, mapped to what they produced, so a retried
    /// seed replays the *same* ids rather than growing a second copy of the
    /// plan. Storing the ids (not just the key) is what makes the replay usable:
    /// the caller that retried still needs the node list it lost.
    applied_seeds: BTreeMap<String, Vec<NodeId>>,
}

impl TaskPlan {
    /// An empty plan.
    #[must_use]
    pub fn new(objective: impl Into<String>, mode: PlanMode, coordinator: ActorId) -> Self {
        Self {
            objective: objective.into(),
            mode,
            coordinator,
            version: 0,
            nodes: BTreeMap::new(),
            next_id: 1,
            applied_seeds: BTreeMap::new(),
        }
    }

    /// What the plan is for.
    #[must_use]
    pub fn objective(&self) -> &str {
        &self.objective
    }

    /// How strictly it is run.
    #[must_use]
    pub fn mode(&self) -> PlanMode {
        self.mode
    }

    /// Who seeded it.
    #[must_use]
    pub fn coordinator(&self) -> &ActorId {
        &self.coordinator
    }

    /// Hand the plan to a new coordinator. Used on re-election; the graph is
    /// untouched, only who owns the gates that have no other owner.
    pub fn set_coordinator(&mut self, coordinator: ActorId) {
        if self.coordinator != coordinator {
            self.coordinator = coordinator;
            self.version += 1;
        }
    }

    /// How many mutations have been accepted.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// One node, if it exists.
    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&TaskNode> {
        self.nodes.get(&id)
    }

    /// Every node, in id order.
    pub fn nodes(&self) -> impl Iterator<Item = &TaskNode> {
        self.nodes.values()
    }

    /// How many nodes the plan holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the plan holds no nodes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Whether every node has reached a terminal state — the plan is over.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.nodes.values().all(TaskNode::is_terminal)
    }

    /// The nodes that wait on `id`, in id order.
    #[must_use]
    pub fn dependents(&self, id: NodeId) -> Vec<NodeId> {
        self.nodes
            .values()
            .filter(|node| node.upstream.contains(&id))
            .map(|node| node.id)
            .collect()
    }

    /// Whether `from` can reach `to` by walking dependency edges upward. Used to
    /// reject an edge that would close a cycle *before* it is written.
    #[must_use]
    pub fn depends_transitively_on(&self, from: NodeId, to: NodeId) -> bool {
        let mut seen = BTreeSet::new();
        let mut stack = vec![from];
        while let Some(current) = stack.pop() {
            if current == to && current != from {
                return true;
            }
            if !seen.insert(current) {
                continue;
            }
            if let Some(node) = self.nodes.get(&current) {
                for up in &node.upstream {
                    if *up == to {
                        return true;
                    }
                    stack.push(*up);
                }
            }
        }
        false
    }

    // --- crate-internal mutation helpers -----------------------------------
    //
    // Kept private so every state change goes through `ops`, where the rules
    // live. A public setter here would be a way to bypass them.

    /// Mint an id and insert a node built from `spec`.
    pub(crate) fn insert(
        &mut self,
        spec: &NodeSpec,
        owner: ActorId,
        upstream: Vec<NodeId>,
    ) -> NodeId {
        let id = NodeId(self.next_id);
        self.next_id += 1;
        let mut upstream = upstream;
        upstream.sort_unstable();
        upstream.dedup();
        self.nodes.insert(
            id,
            TaskNode {
                id,
                title: spec.title.clone(),
                prompt: spec.prompt.clone(),
                kind: spec.kind,
                owner,
                status: TaskStatus::Pending,
                upstream,
                artifact: None,
                reclaims: 0,
            },
        );
        id
    }

    pub(crate) fn node_mut(&mut self, id: NodeId) -> Option<&mut TaskNode> {
        self.nodes.get_mut(&id)
    }

    pub(crate) fn bump_version(&mut self) {
        self.version += 1;
    }

    pub(crate) fn replayed_seed(&self, key: &str) -> Option<&[NodeId]> {
        self.applied_seeds.get(key).map(Vec::as_slice)
    }

    pub(crate) fn record_seed(&mut self, key: String, produced: Vec<NodeId>) {
        self.applied_seeds.insert(key, produced);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> TaskPlan {
        TaskPlan::new("ship it", PlanMode::Light, ActorId::new("lead"))
    }

    #[test]
    fn ids_are_minted_in_creation_order() {
        let mut plan = plan();
        let a = plan.insert(&NodeSpec::task("a", "do a"), "lead".into(), vec![]);
        let b = plan.insert(&NodeSpec::task("b", "do b"), "lead".into(), vec![a]);
        assert_eq!(a.get(), 1);
        assert_eq!(b.get(), 2);
        assert!(a < b);
    }

    #[test]
    fn upstream_is_sorted_and_deduplicated_on_write() {
        let mut plan = plan();
        let a = plan.insert(&NodeSpec::task("a", ""), "lead".into(), vec![]);
        let b = plan.insert(&NodeSpec::task("b", ""), "lead".into(), vec![]);
        let c = plan.insert(&NodeSpec::task("c", ""), "lead".into(), vec![b, a, b]);
        assert_eq!(plan.node(c).unwrap().upstream, vec![a, b]);
    }

    #[test]
    fn dependents_are_derived_not_stored() {
        let mut plan = plan();
        let a = plan.insert(&NodeSpec::task("a", ""), "lead".into(), vec![]);
        let b = plan.insert(&NodeSpec::task("b", ""), "lead".into(), vec![a]);
        let c = plan.insert(&NodeSpec::task("c", ""), "lead".into(), vec![a]);
        assert_eq!(plan.dependents(a), vec![b, c]);
        assert!(plan.dependents(b).is_empty());
    }

    #[test]
    fn transitive_dependency_is_detected_through_a_chain() {
        let mut plan = plan();
        let a = plan.insert(&NodeSpec::task("a", ""), "lead".into(), vec![]);
        let b = plan.insert(&NodeSpec::task("b", ""), "lead".into(), vec![a]);
        let c = plan.insert(&NodeSpec::task("c", ""), "lead".into(), vec![b]);
        assert!(plan.depends_transitively_on(c, a));
        assert!(!plan.depends_transitively_on(a, c));
    }

    #[test]
    fn a_terminal_status_reports_itself_terminal() {
        assert!(!TaskStatus::Pending.is_terminal());
        assert!(!TaskStatus::Assigned {
            assignee: "w".into()
        }
        .is_terminal());
        assert!(TaskStatus::Complete.is_terminal());
        assert!(TaskStatus::Failed {
            reason: "boom".into()
        }
        .is_terminal());
        assert!(TaskStatus::Abandoned {
            reason: "moot".into()
        }
        .is_terminal());
    }

    #[test]
    fn an_empty_plan_is_settled_vacuously_but_not_after_a_node_is_added() {
        let mut plan = plan();
        assert!(plan.is_settled());
        plan.insert(&NodeSpec::task("a", ""), "lead".into(), vec![]);
        assert!(!plan.is_settled());
    }

    #[test]
    fn re_electing_the_same_coordinator_does_not_bump_the_version() {
        let mut plan = plan();
        let before = plan.version();
        plan.set_coordinator("lead".into());
        assert_eq!(plan.version(), before);
        plan.set_coordinator("second".into());
        assert_eq!(plan.version(), before + 1);
    }
}
