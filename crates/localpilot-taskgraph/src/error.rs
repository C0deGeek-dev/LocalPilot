//! Why a mutation was refused.
//!
//! Every variant is a *reportable* outcome, never a panic: a plan is mutated by
//! a language model through a tool, so "you cannot do that" has to come back as
//! text the model can act on. The messages are written for that reader — they
//! say what was refused and what would work instead.

use crate::plan::{ActorId, NodeId};

/// A refused mutation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PlanError {
    /// No node carries this id.
    #[error("no task {0} in this plan")]
    UnknownNode(NodeId),

    /// The actor does not own the node it tried to mutate.
    #[error(
        "task {node} belongs to {owner}, not {actor} — ask {owner} to change it, or work on a task you own"
    )]
    NotOwner {
        /// The node that was targeted.
        node: NodeId,
        /// Who tried.
        actor: ActorId,
        /// Who may.
        owner: ActorId,
    },

    /// The actor is not the one the node is assigned to.
    #[error("task {node} is assigned to {assignee}, not {actor}")]
    WrongAssignee {
        /// The node that was targeted.
        node: NodeId,
        /// Who tried.
        actor: ActorId,
        /// Who holds the assignment.
        assignee: ActorId,
    },

    /// The node was not assigned to anyone, so it cannot be completed.
    #[error("task {0} is not assigned to anyone — it has to be dispatched before it can finish")]
    NotAssigned(NodeId),

    /// The node already reached a terminal state.
    #[error("task {node} already finished ({state}) — finished tasks do not change")]
    AlreadyTerminal {
        /// The node that was targeted.
        node: NodeId,
        /// The terminal state it is in.
        state: &'static str,
    },

    /// The edge would close a cycle.
    #[error(
        "task {dependent} cannot wait on task {dependency}: {dependency} already waits on \
         {dependent}, so nothing would ever start"
    )]
    WouldCycle {
        /// The node that would gain a dependency.
        dependent: NodeId,
        /// The node it would wait on.
        dependency: NodeId,
    },

    /// A dependency index in a batch pointed outside the batch.
    #[error("a new task depends on position {index} of a batch that has only {len}")]
    BatchIndexOutOfRange {
        /// The offending index.
        index: usize,
        /// How many entries the batch actually has.
        len: usize,
    },

    /// A batch of new tasks described a loop among themselves.
    #[error(
        "the new tasks depend on each other in a loop (position {index}), so none of them could \
         ever start — check the `depends_on` positions"
    )]
    BatchCycle {
        /// A position on the loop.
        index: usize,
    },

    /// A mutation that must add work added none.
    #[error("{0} needs at least one new task")]
    EmptyBatch(&'static str),

    /// A completion arrived without the artifact this mode requires.
    #[error(
        "task {node} finished without a handoff: {missing}. Downstream tasks read the handoff \
         instead of redoing this work, so an empty one costs the plan the whole task."
    )]
    IncompleteArtifact {
        /// The node that was targeted.
        node: NodeId,
        /// Which part is missing, phrased for the model.
        missing: &'static str,
    },

    /// A review gate tried to close without reviewing anything.
    #[error(
        "gate {node} closed without saying what it reviewed. A gate must record how it checked \
         its inputs and cite at least one piece of evidence, or raise findings and inject \
         follow-up work."
    )]
    RubberStampGate {
        /// The gate that was targeted.
        node: NodeId,
    },

    /// The mutation targeted a node of the wrong kind.
    #[error("task {node} is not a review gate")]
    NotAGate {
        /// The node that was targeted.
        node: NodeId,
    },

    /// The node cannot be dispatched yet.
    #[error("task {node} still waits on {waiting_on:?} — those have to finish first")]
    NotReady {
        /// The node that was targeted.
        node: NodeId,
        /// What it is still waiting on.
        waiting_on: Vec<NodeId>,
    },

    /// The node is already in flight somewhere else.
    #[error("task {node} is already being worked on by {assignee}")]
    AlreadyAssigned {
        /// The node that was targeted.
        node: NodeId,
        /// Who holds it.
        assignee: ActorId,
    },

    /// The node can never become ready: something it waits on ended badly.
    #[error("task {node} can never start — it waits on {blocked_by:?}, which did not complete")]
    Blocked {
        /// The node that was targeted.
        node: NodeId,
        /// The upstream nodes that ended without completing.
        blocked_by: Vec<NodeId>,
    },

    /// A salvage exhausted its reclaim budget.
    #[error(
        "task {node} has been reclaimed {reclaims} times (limit {limit}) — it is failing, not \
         unlucky; it will not be requeued again"
    )]
    ReclaimBudgetExhausted {
        /// The node that was targeted.
        node: NodeId,
        /// How many reclaims it has had.
        reclaims: u32,
        /// The ceiling.
        limit: u32,
    },
}
