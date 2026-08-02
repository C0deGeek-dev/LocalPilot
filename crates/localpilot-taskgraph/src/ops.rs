//! Every way a plan may change, and the rules each change has to satisfy.
//!
//! The graph has no public setters. A plan is mutated only through the four
//! operations here — [`seed`], [`expand_node`], [`complete_node`],
//! [`inject_from_gate`] — plus the lifecycle operations a supervisor needs
//! ([`fail_node`], [`abandon_node`], [`salvage_assignment`]). That is deliberate:
//! the rules are the point, and a plan a caller could edit field-by-field would
//! have none of them.
//!
//! Four rules hold across all of them:
//!
//! 1. **Ownership.** Only a node's owner or its current assignee may change it.
//!    Without this, one worker can rewrite another's subtree and neither notices.
//! 2. **Acyclicity.** Every added edge is checked against the existing graph
//!    *before* it is written, so a plan can never be stored in a state where
//!    nothing can start.
//! 3. **Terminality.** A finished node does not change. Rework enters the plan
//!    as new nodes, which keeps the record of what went wrong.
//! 4. **Honest completion.** A completion carries a handoff. In deep mode it
//!    also has to say what it did *not* check, and a review gate has to say how
//!    it reviewed and cite something — a gate that waves work through is worse
//!    than no gate, because the plan then claims a review happened.

use crate::artifact::HandoffArtifact;
use crate::error::PlanError;
use crate::plan::{ActorId, NodeId, NodeKind, NodeSpec, TaskPlan, TaskStatus};

/// What a seed produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seeded {
    /// The nodes created, in creation order.
    pub nodes: Vec<NodeId>,
    /// The final review gate, in deep mode.
    pub gate: Option<NodeId>,
    /// Whether this was a replay of an earlier seed with the same key, in which
    /// case nothing was created and `nodes` is what the first call produced.
    pub replayed: bool,
}

/// What an expansion produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expanded {
    /// The children created, in creation order.
    pub children: Vec<NodeId>,
    /// The gate inserted between the children and the expanded node, in deep
    /// mode.
    pub gate: Option<NodeId>,
}

/// The outcome of salvaging a departed worker's assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Salvage {
    /// The task went back into the ready pool for someone else to pick up.
    Requeued {
        /// How many times it has now been reclaimed.
        reclaims: u32,
    },
    /// The task had used up its reclaim budget and was failed instead. A task
    /// that keeps outliving its workers is failing, not unlucky.
    Exhausted {
        /// How many reclaims it had.
        reclaims: u32,
        /// The ceiling it hit.
        limit: u32,
    },
}

/// Seed a plan with a batch of tasks.
///
/// `key` makes the call idempotent: a caller that retried after a lost response
/// gets the *first* call's node ids back and creates nothing. Without it a
/// retried seed silently doubles the plan, which is both expensive and very hard
/// to see afterwards.
///
/// In deep mode a final review gate is appended over the *whole* batch, so a
/// deep plan cannot finish without something having reviewed it.
///
/// # Errors
/// [`PlanError::EmptyBatch`] for an empty batch,
/// [`PlanError::BatchIndexOutOfRange`] for an in-batch dependency that points
/// nowhere, or [`PlanError::BatchCycle`] for a batch that loops on itself.
pub fn seed(
    plan: &mut TaskPlan,
    actor: &ActorId,
    key: &str,
    specs: &[NodeSpec],
) -> Result<Seeded, PlanError> {
    if let Some(existing) = plan.replayed_seed(key) {
        // Split the record back into the shape the first call returned — tasks
        // in `nodes`, the auto-inserted gate in `gate` — so a caller cannot tell
        // a replay from the original except by the flag.
        let (gates, nodes): (Vec<NodeId>, Vec<NodeId>) = existing
            .iter()
            .partition(|id| plan.node(**id).is_some_and(|n| n.kind == NodeKind::Gate));
        return Ok(Seeded {
            nodes,
            gate: gates.first().copied(),
            replayed: true,
        });
    }
    if specs.is_empty() {
        return Err(PlanError::EmptyBatch("seeding a plan"));
    }
    validate_batch(specs)?;

    let created = insert_batch(plan, actor, specs, &[]);
    let gate = if plan.mode().is_deep() {
        Some(append_gate(
            plan,
            actor,
            "Plan review",
            "Review every finished task this gate waits on. Say how you checked them and cite \
             what you looked at. If anything is unfinished, wrong, or unverified, raise it and \
             inject follow-up work instead of passing.",
            &created,
        ))
    } else {
        None
    };

    let mut produced = created.clone();
    produced.extend(gate);
    plan.record_seed(key.to_string(), produced);
    plan.bump_version();
    Ok(Seeded {
        nodes: created,
        gate,
        replayed: false,
    })
}

/// Decompose a node into children it now waits on.
///
/// The expanded node is *not* replaced — it becomes a join. Its children inherit
/// its dependencies, it comes to depend on them, and it goes back to pending so
/// it runs again once they are done. That is what keeps expansion cheap: nothing
/// downstream of the expanded node has to be rewired, because the node it
/// depends on is still there.
///
/// In deep mode a review gate is inserted between the children and the join, so
/// a fan-out is reviewed before its results are synthesised.
///
/// # Errors
/// [`PlanError::UnknownNode`], [`PlanError::AlreadyTerminal`],
/// [`PlanError::NotOwner`], [`PlanError::EmptyBatch`],
/// [`PlanError::BatchIndexOutOfRange`], or [`PlanError::BatchCycle`].
pub fn expand_node(
    plan: &mut TaskPlan,
    node: NodeId,
    actor: &ActorId,
    specs: &[NodeSpec],
) -> Result<Expanded, PlanError> {
    let target = require_mutable(plan, node, actor)?;
    if specs.is_empty() {
        return Err(PlanError::EmptyBatch("expanding a task"));
    }
    validate_batch(specs)?;

    let inherited = target.upstream.clone();
    let children = insert_batch(plan, actor, specs, &inherited);

    let gate = if plan.mode().is_deep() {
        Some(append_gate(
            plan,
            actor,
            "Sub-plan review",
            "Review the tasks this gate waits on before their results are combined. Say how you \
             checked them and cite what you looked at. Raise gaps and inject follow-up work \
             rather than passing them along.",
            &children,
        ))
    } else {
        None
    };

    // The join waits on *every* child, not merely on the batch's last nodes.
    // Ordering would be satisfied either way — a sink transitively depends on
    // the rest — but readiness is not the point here: what the join waits on is
    // what gets hydrated into its input, and a synthesis step that cannot see
    // half of what it is synthesising is worse than no synthesis step. The same
    // reasoning gives a gate the whole batch to review.
    let waits_on = gate.map_or_else(|| children.clone(), |gate| vec![gate]);
    // Every new edge is checked against the graph as it stands, even though this
    // shape cannot cycle by construction: the check is cheap and the invariant
    // is worth more than the microseconds.
    for up in &waits_on {
        ensure_no_cycle(plan, node, *up)?;
    }
    if let Some(target) = plan.node_mut(node) {
        target.upstream = waits_on;
        target.upstream.sort_unstable();
        target.upstream.dedup();
        target.status = TaskStatus::Pending;
    }
    plan.bump_version();
    Ok(Expanded { children, gate })
}

/// Finish a node with the handoff everything downstream will read.
///
/// # Errors
/// [`PlanError::UnknownNode`], [`PlanError::AlreadyTerminal`],
/// [`PlanError::NotAssigned`], [`PlanError::WrongAssignee`],
/// [`PlanError::IncompleteArtifact`], or [`PlanError::RubberStampGate`].
pub fn complete_node(
    plan: &mut TaskPlan,
    node: NodeId,
    actor: &ActorId,
    artifact: HandoffArtifact,
) -> Result<(), PlanError> {
    let target = plan.node(node).ok_or(PlanError::UnknownNode(node))?;
    if target.is_terminal() {
        return Err(PlanError::AlreadyTerminal {
            node,
            state: target.status.label(),
        });
    }
    match target.assignee() {
        None => return Err(PlanError::NotAssigned(node)),
        Some(assignee) if assignee != actor => {
            return Err(PlanError::WrongAssignee {
                node,
                actor: actor.clone(),
                assignee: assignee.clone(),
            })
        }
        Some(_) => {}
    }
    validate_artifact(node, target.kind, plan.mode().is_deep(), &artifact)?;

    if let Some(target) = plan.node_mut(node) {
        target.status = TaskStatus::Complete;
        target.artifact = Some(artifact);
    }
    plan.bump_version();
    Ok(())
}

/// A gate raises findings by adding work.
///
/// The injected nodes inherit the gate's own inputs — they are remediation for
/// what the gate was reviewing — and the gate comes to wait on them, so it
/// re-reviews once they are done. A gate that could inject without re-reviewing
/// would be a gate that never actually approves anything.
///
/// # Errors
/// [`PlanError::UnknownNode`], [`PlanError::NotAGate`],
/// [`PlanError::AlreadyTerminal`], [`PlanError::NotOwner`],
/// [`PlanError::EmptyBatch`], [`PlanError::BatchIndexOutOfRange`],
/// [`PlanError::BatchCycle`], or [`PlanError::WouldCycle`].
pub fn inject_from_gate(
    plan: &mut TaskPlan,
    gate: NodeId,
    actor: &ActorId,
    specs: &[NodeSpec],
) -> Result<Vec<NodeId>, PlanError> {
    let target = require_mutable(plan, gate, actor)?;
    if target.kind != NodeKind::Gate {
        return Err(PlanError::NotAGate { node: gate });
    }
    if specs.is_empty() {
        return Err(PlanError::EmptyBatch("a gate raising findings"));
    }
    validate_batch(specs)?;

    let inherited = target.upstream.clone();
    let injected = insert_batch(plan, actor, specs, &inherited);
    for up in &injected {
        ensure_no_cycle(plan, gate, *up)?;
    }
    if let Some(target) = plan.node_mut(gate) {
        target.upstream.extend(injected.iter().copied());
        target.upstream.sort_unstable();
        target.upstream.dedup();
        target.status = TaskStatus::Pending;
    }
    plan.bump_version();
    Ok(injected)
}

/// Finish a node badly. Terminal — rework re-enters the plan as new nodes.
///
/// # Errors
/// [`PlanError::UnknownNode`], [`PlanError::AlreadyTerminal`], or
/// [`PlanError::NotOwner`].
pub fn fail_node(
    plan: &mut TaskPlan,
    node: NodeId,
    actor: &ActorId,
    reason: impl Into<String>,
) -> Result<(), PlanError> {
    require_mutable(plan, node, actor)?;
    if let Some(target) = plan.node_mut(node) {
        target.status = TaskStatus::Failed {
            reason: reason.into(),
        };
    }
    plan.bump_version();
    Ok(())
}

/// Drop a node the plan has moved past. Only its owner may: abandoning someone
/// else's work is the one mutation with no way back.
///
/// # Errors
/// [`PlanError::UnknownNode`], [`PlanError::AlreadyTerminal`], or
/// [`PlanError::NotOwner`].
pub fn abandon_node(
    plan: &mut TaskPlan,
    node: NodeId,
    actor: &ActorId,
    reason: impl Into<String>,
) -> Result<(), PlanError> {
    let target = plan.node(node).ok_or(PlanError::UnknownNode(node))?;
    if target.is_terminal() {
        return Err(PlanError::AlreadyTerminal {
            node,
            state: target.status.label(),
        });
    }
    if &target.owner != actor {
        return Err(PlanError::NotOwner {
            node,
            actor: actor.clone(),
            owner: target.owner.clone(),
        });
    }
    if let Some(target) = plan.node_mut(node) {
        target.status = TaskStatus::Abandoned {
            reason: reason.into(),
        };
    }
    plan.bump_version();
    Ok(())
}

/// Take one in-flight task back from a worker that is gone.
///
/// Requeues it for someone else unless it has already been reclaimed `limit`
/// times, in which case it fails loudly instead: a task that keeps outliving its
/// workers is the task's problem, and quietly requeuing it forever turns one bad
/// node into a plan that never finishes.
///
/// # Errors
/// [`PlanError::UnknownNode`] or [`PlanError::AlreadyTerminal`].
pub fn salvage_assignment(
    plan: &mut TaskPlan,
    node: NodeId,
    limit: u32,
) -> Result<Salvage, PlanError> {
    let target = plan.node(node).ok_or(PlanError::UnknownNode(node))?;
    if target.is_terminal() {
        return Err(PlanError::AlreadyTerminal {
            node,
            state: target.status.label(),
        });
    }
    let reclaims = target.reclaims;
    let outcome = if reclaims >= limit {
        Salvage::Exhausted { reclaims, limit }
    } else {
        Salvage::Requeued {
            reclaims: reclaims + 1,
        }
    };
    if let Some(target) = plan.node_mut(node) {
        match &outcome {
            Salvage::Requeued { reclaims } => {
                target.reclaims = *reclaims;
                target.status = TaskStatus::Pending;
            }
            Salvage::Exhausted { reclaims, limit } => {
                target.status = TaskStatus::Failed {
                    reason: format!(
                        "reclaimed {reclaims} times (limit {limit}) after its workers stopped \
                         reporting; the task is failing, not unlucky"
                    ),
                };
            }
        }
    }
    plan.bump_version();
    Ok(outcome)
}

/// Salvage every non-terminal task assigned to `actor`, in id order.
///
/// The sweep a supervisor runs when a worker dies. Returns what happened to each
/// task so the caller can report it rather than guess.
pub fn salvage_actor(plan: &mut TaskPlan, actor: &ActorId, limit: u32) -> Vec<(NodeId, Salvage)> {
    let stranded: Vec<NodeId> = plan
        .nodes()
        .filter(|node| !node.is_terminal() && node.assignee() == Some(actor))
        .map(|node| node.id)
        .collect();
    stranded
        .into_iter()
        .filter_map(|id| {
            salvage_assignment(plan, id, limit)
                .ok()
                .map(|outcome| (id, outcome))
        })
        .collect()
}

// --- shared checks ---------------------------------------------------------

/// Fetch a node that `actor` is allowed to change, or say why not.
fn require_mutable<'a>(
    plan: &'a TaskPlan,
    node: NodeId,
    actor: &ActorId,
) -> Result<&'a crate::plan::TaskNode, PlanError> {
    let target = plan.node(node).ok_or(PlanError::UnknownNode(node))?;
    if target.is_terminal() {
        return Err(PlanError::AlreadyTerminal {
            node,
            state: target.status.label(),
        });
    }
    let permitted = &target.owner == actor || target.assignee() == Some(actor);
    if !permitted {
        return Err(PlanError::NotOwner {
            node,
            actor: actor.clone(),
            owner: target.owner.clone(),
        });
    }
    Ok(target)
}

/// Refuse an edge that would close a cycle.
fn ensure_no_cycle(
    plan: &TaskPlan,
    dependent: NodeId,
    dependency: NodeId,
) -> Result<(), PlanError> {
    if dependency == dependent || plan.depends_transitively_on(dependency, dependent) {
        return Err(PlanError::WouldCycle {
            dependent,
            dependency,
        });
    }
    Ok(())
}

/// Check a batch's in-batch dependencies before anything is created, so a bad
/// batch leaves the plan untouched rather than half-built.
///
/// The loop check matters as much as the range check and is easier to miss: a
/// batch is the one place a caller names edges among nodes that do not exist
/// yet, so the graph's own acyclicity check has nothing to look at. `[0 after 1,
/// 1 after 0]` would otherwise be written straight into the plan and deadlock it
/// permanently.
fn validate_batch(specs: &[NodeSpec]) -> Result<(), PlanError> {
    for spec in specs {
        for index in &spec.depends_on_batch {
            if *index >= specs.len() {
                return Err(PlanError::BatchIndexOutOfRange {
                    index: *index,
                    len: specs.len(),
                });
            }
        }
    }
    // Depth-first over the batch's own index graph. `visiting` is the current
    // path, so an edge back onto it is a loop; `settled` keeps this linear.
    let mut settled = vec![false; specs.len()];
    let mut visiting = vec![false; specs.len()];
    for start in 0..specs.len() {
        if settled[start] {
            continue;
        }
        // An explicit stack rather than recursion: a model can describe a very
        // long chain, and blowing the stack on bad input is not an option.
        let mut stack = vec![(start, 0usize)];
        visiting[start] = true;
        while let Some((node, edge)) = stack.pop() {
            match specs[node].depends_on_batch.get(edge) {
                None => {
                    visiting[node] = false;
                    settled[node] = true;
                }
                Some(&next) => {
                    stack.push((node, edge + 1));
                    if visiting[next] {
                        return Err(PlanError::BatchCycle { index: next });
                    }
                    if !settled[next] {
                        visiting[next] = true;
                        stack.push((next, 0));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Create a batch, resolving in-batch dependencies to real ids as it goes.
///
/// A forward reference (position 0 depending on position 2) resolves because the
/// batch is inserted first with only its inherited dependencies, then the
/// in-batch edges are written; that keeps the caller from having to order its
/// own list topologically.
fn insert_batch(
    plan: &mut TaskPlan,
    owner: &ActorId,
    specs: &[NodeSpec],
    inherited: &[NodeId],
) -> Vec<NodeId> {
    let created: Vec<NodeId> = specs
        .iter()
        .map(|spec| plan.insert(spec, owner.clone(), inherited.to_vec()))
        .collect();
    for (spec, id) in specs.iter().zip(&created) {
        let extra: Vec<NodeId> = spec
            .depends_on_batch
            .iter()
            .filter_map(|index| created.get(*index).copied())
            .filter(|dep| dep != id)
            .collect();
        if extra.is_empty() {
            continue;
        }
        if let Some(node) = plan.node_mut(*id) {
            node.upstream.extend(extra);
            node.upstream.sort_unstable();
            node.upstream.dedup();
        }
    }
    created
}

/// Append a review gate over `over`.
fn append_gate(
    plan: &mut TaskPlan,
    owner: &ActorId,
    title: &str,
    prompt: &str,
    over: &[NodeId],
) -> NodeId {
    plan.insert(&NodeSpec::gate(title, prompt), owner.clone(), over.to_vec())
}

/// Refuse a completion that hands nothing on.
fn validate_artifact(
    node: NodeId,
    kind: NodeKind,
    deep: bool,
    artifact: &HandoffArtifact,
) -> Result<(), PlanError> {
    if artifact.findings.trim().is_empty() {
        return Err(PlanError::IncompleteArtifact {
            node,
            missing: "it records no findings at all",
        });
    }
    if deep && artifact.what_i_did_not_check.trim().is_empty() {
        return Err(PlanError::IncompleteArtifact {
            node,
            missing: "it does not say what was left unchecked, which this plan requires",
        });
    }
    if kind == NodeKind::Gate {
        let reviewed = !artifact.validation.trim().is_empty();
        let cited = artifact.evidence.iter().any(|e| !e.trim().is_empty());
        if !reviewed || !cited {
            return Err(PlanError::RubberStampGate { node });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::Confidence;
    use crate::plan::PlanMode;
    use crate::schedule::dispatch;

    fn lead() -> ActorId {
        ActorId::new("lead")
    }

    fn worker() -> ActorId {
        ActorId::new("worker")
    }

    fn plan(mode: PlanMode) -> TaskPlan {
        TaskPlan::new("ship the thing", mode, lead())
    }

    fn good(findings: &str) -> HandoffArtifact {
        HandoffArtifact::new(findings, Confidence::new(0.8)).with_gap("the other platform")
    }

    fn gate_report() -> HandoffArtifact {
        HandoffArtifact::new("all inputs check out", Confidence::new(0.9))
            .with_gap("the integration path")
            .with_validation("re-read each handoff against the task it answered")
            .with_evidence("src/lib.rs:1")
    }

    /// Finish `node` cleanly, whoever it belongs to.
    fn run_to_completion(plan: &mut TaskPlan, node: NodeId) {
        dispatch(plan, node, &worker()).unwrap();
        complete_node(plan, node, &worker(), good("done")).unwrap();
    }

    fn three() -> Vec<NodeSpec> {
        vec![
            NodeSpec::task("a", "do a"),
            NodeSpec::task("b", "do b"),
            NodeSpec::task("c", "do c").after(0),
        ]
    }

    #[test]
    fn seeding_creates_the_batch_and_its_in_batch_edges() {
        let mut plan = plan(PlanMode::Light);
        let seeded = seed(&mut plan, &lead(), "k1", &three()).unwrap();
        assert_eq!(seeded.nodes.len(), 3);
        assert!(seeded.gate.is_none());
        let c = plan.node(seeded.nodes[2]).unwrap();
        assert_eq!(c.upstream, vec![seeded.nodes[0]]);
        assert_eq!(plan.version(), 1);
    }

    #[test]
    fn a_replayed_seed_creates_nothing_and_returns_the_first_ids() {
        let mut plan = plan(PlanMode::Light);
        let first = seed(&mut plan, &lead(), "k1", &three()).unwrap();
        let version = plan.version();
        let again = seed(&mut plan, &lead(), "k1", &three()).unwrap();
        assert!(again.replayed);
        assert_eq!(again.nodes, first.nodes);
        assert_eq!(plan.len(), 3);
        assert_eq!(plan.version(), version, "a replay is not a mutation");
    }

    #[test]
    fn deep_mode_seeds_a_final_gate_over_the_whole_batch() {
        let mut plan = plan(PlanMode::Deep);
        let seeded = seed(&mut plan, &lead(), "k1", &three()).unwrap();
        let gate = seeded.gate.expect("deep mode gates the seed");
        let gate_node = plan.node(gate).unwrap();
        assert_eq!(gate_node.kind, NodeKind::Gate);
        // Every seeded task, including `a`, which `c` already waits on: a review
        // reads what it is reviewing, and only upstream nodes are hydrated.
        assert_eq!(gate_node.upstream, seeded.nodes);
    }

    #[test]
    fn an_empty_seed_is_refused() {
        let mut plan = plan(PlanMode::Light);
        assert!(matches!(
            seed(&mut plan, &lead(), "k1", &[]),
            Err(PlanError::EmptyBatch(_))
        ));
    }

    #[test]
    fn an_out_of_range_batch_index_leaves_the_plan_untouched() {
        let mut plan = plan(PlanMode::Light);
        let specs = vec![NodeSpec::task("a", "do a").after(4)];
        assert!(matches!(
            seed(&mut plan, &lead(), "k1", &specs),
            Err(PlanError::BatchIndexOutOfRange { index: 4, len: 1 })
        ));
        assert!(plan.is_empty());
    }

    #[test]
    fn expansion_turns_the_node_into_a_join_and_keeps_downstream_edges() {
        let mut plan = plan(PlanMode::Light);
        let seeded = seed(&mut plan, &lead(), "k1", &three()).unwrap();
        let (a, c) = (seeded.nodes[0], seeded.nodes[2]);
        dispatch(&mut plan, a, &worker()).unwrap();
        let expanded = expand_node(
            &mut plan,
            a,
            &worker(),
            &[
                NodeSpec::task("a1", "part one"),
                NodeSpec::task("a2", "part two"),
            ],
        )
        .unwrap();

        assert_eq!(expanded.children.len(), 2);
        assert_eq!(plan.node(a).unwrap().upstream, expanded.children);
        assert_eq!(plan.node(a).unwrap().status, TaskStatus::Pending);
        // Nothing downstream needed rewiring.
        assert_eq!(plan.node(c).unwrap().upstream, vec![a]);
    }

    #[test]
    fn children_inherit_the_expanded_nodes_dependencies() {
        let mut plan = plan(PlanMode::Light);
        let seeded = seed(&mut plan, &lead(), "k1", &three()).unwrap();
        let (a, c) = (seeded.nodes[0], seeded.nodes[2]);
        run_to_completion(&mut plan, a);
        dispatch(&mut plan, c, &worker()).unwrap();
        let expanded =
            expand_node(&mut plan, c, &worker(), &[NodeSpec::task("c1", "part")]).unwrap();
        assert_eq!(plan.node(expanded.children[0]).unwrap().upstream, vec![a]);
    }

    #[test]
    fn deep_mode_gates_an_expansion() {
        let mut plan = plan(PlanMode::Deep);
        let seeded = seed(&mut plan, &lead(), "k1", &three()).unwrap();
        let a = seeded.nodes[0];
        dispatch(&mut plan, a, &worker()).unwrap();
        let expanded =
            expand_node(&mut plan, a, &worker(), &[NodeSpec::task("a1", "part")]).unwrap();
        let gate = expanded.gate.expect("deep mode gates an expansion");
        assert_eq!(plan.node(gate).unwrap().upstream, expanded.children);
        assert_eq!(plan.node(a).unwrap().upstream, vec![gate]);
    }

    #[test]
    fn a_stranger_cannot_expand_someone_elses_task() {
        let mut plan = plan(PlanMode::Light);
        let seeded = seed(&mut plan, &lead(), "k1", &three()).unwrap();
        let err = expand_node(
            &mut plan,
            seeded.nodes[0],
            &ActorId::new("stranger"),
            &[NodeSpec::task("x", "")],
        )
        .unwrap_err();
        assert!(matches!(err, PlanError::NotOwner { .. }));
        assert_eq!(plan.len(), 3, "the refused batch was not created");
    }

    #[test]
    fn completing_requires_the_assignment() {
        let mut plan = plan(PlanMode::Light);
        let seeded = seed(&mut plan, &lead(), "k1", &three()).unwrap();
        let a = seeded.nodes[0];
        assert!(matches!(
            complete_node(&mut plan, a, &worker(), good("done")),
            Err(PlanError::NotAssigned(_))
        ));
        dispatch(&mut plan, a, &worker()).unwrap();
        assert!(matches!(
            complete_node(&mut plan, a, &ActorId::new("other"), good("done")),
            Err(PlanError::WrongAssignee { .. })
        ));
        complete_node(&mut plan, a, &worker(), good("done")).unwrap();
        assert_eq!(plan.node(a).unwrap().status, TaskStatus::Complete);
    }

    #[test]
    fn a_finished_task_does_not_change() {
        let mut plan = plan(PlanMode::Light);
        let seeded = seed(&mut plan, &lead(), "k1", &three()).unwrap();
        let a = seeded.nodes[0];
        dispatch(&mut plan, a, &worker()).unwrap();
        complete_node(&mut plan, a, &worker(), good("done")).unwrap();
        assert!(matches!(
            complete_node(&mut plan, a, &worker(), good("done again")),
            Err(PlanError::AlreadyTerminal { .. })
        ));
        assert!(matches!(
            expand_node(&mut plan, a, &worker(), &[NodeSpec::task("x", "")]),
            Err(PlanError::AlreadyTerminal { .. })
        ));
    }

    #[test]
    fn deep_mode_refuses_a_completion_that_omits_the_gap() {
        let mut plan = plan(PlanMode::Deep);
        let seeded = seed(&mut plan, &lead(), "k1", &three()).unwrap();
        let a = seeded.nodes[0];
        dispatch(&mut plan, a, &worker()).unwrap();
        let bare = HandoffArtifact::new("did it", Confidence::new(0.9));
        assert!(matches!(
            complete_node(&mut plan, a, &worker(), bare),
            Err(PlanError::IncompleteArtifact { .. })
        ));
    }

    #[test]
    fn light_mode_accepts_a_completion_without_the_gap() {
        let mut plan = plan(PlanMode::Light);
        let seeded = seed(&mut plan, &lead(), "k1", &three()).unwrap();
        let a = seeded.nodes[0];
        dispatch(&mut plan, a, &worker()).unwrap();
        let bare = HandoffArtifact::new("did it", Confidence::new(0.9));
        complete_node(&mut plan, a, &worker(), bare).unwrap();
    }

    #[test]
    fn an_empty_findings_field_is_refused_in_every_mode() {
        let mut plan = plan(PlanMode::Light);
        let seeded = seed(&mut plan, &lead(), "k1", &three()).unwrap();
        let a = seeded.nodes[0];
        dispatch(&mut plan, a, &worker()).unwrap();
        let blank = HandoffArtifact::new("   ", Confidence::new(0.9));
        assert!(matches!(
            complete_node(&mut plan, a, &worker(), blank),
            Err(PlanError::IncompleteArtifact { .. })
        ));
    }

    #[test]
    fn a_gate_cannot_rubber_stamp() {
        let mut plan = plan(PlanMode::Deep);
        let seeded = seed(&mut plan, &lead(), "k1", &three()).unwrap();
        let gate = seeded.gate.unwrap();
        // Finish the gate's inputs — and `a`, which `c` waits on — so the gate
        // can be dispatched.
        for id in [seeded.nodes[0], seeded.nodes[1], seeded.nodes[2]] {
            run_to_completion(&mut plan, id);
        }
        dispatch(&mut plan, gate, &lead()).unwrap();
        let waved_through =
            HandoffArtifact::new("looks fine", Confidence::FULL).with_gap("nothing");
        assert!(matches!(
            complete_node(&mut plan, gate, &lead(), waved_through),
            Err(PlanError::RubberStampGate { .. })
        ));
        complete_node(&mut plan, gate, &lead(), gate_report()).unwrap();
    }

    #[test]
    fn a_gate_injects_work_and_goes_back_to_pending() {
        let mut plan = plan(PlanMode::Deep);
        let seeded = seed(&mut plan, &lead(), "k1", &three()).unwrap();
        let gate = seeded.gate.unwrap();
        let injected = inject_from_gate(
            &mut plan,
            gate,
            &lead(),
            &[NodeSpec::task("fix", "address the finding")],
        )
        .unwrap();
        let gate_node = plan.node(gate).unwrap();
        assert!(gate_node.upstream.contains(&injected[0]));
        assert_eq!(gate_node.status, TaskStatus::Pending);
        // The injected task inherited the gate's inputs — the very work the
        // finding is about.
        assert_eq!(plan.node(injected[0]).unwrap().upstream, seeded.nodes);
    }

    #[test]
    fn only_a_gate_may_inject() {
        let mut plan = plan(PlanMode::Deep);
        let seeded = seed(&mut plan, &lead(), "k1", &three()).unwrap();
        assert!(matches!(
            inject_from_gate(
                &mut plan,
                seeded.nodes[0],
                &lead(),
                &[NodeSpec::task("x", "")]
            ),
            Err(PlanError::NotAGate { .. })
        ));
    }

    #[test]
    fn salvage_requeues_until_the_budget_runs_out() {
        let mut plan = plan(PlanMode::Light);
        let seeded = seed(&mut plan, &lead(), "k1", &three()).unwrap();
        let a = seeded.nodes[0];
        for expected in 1..=2 {
            dispatch(&mut plan, a, &worker()).unwrap();
            assert_eq!(
                salvage_assignment(&mut plan, a, 2).unwrap(),
                Salvage::Requeued { reclaims: expected }
            );
            assert_eq!(plan.node(a).unwrap().status, TaskStatus::Pending);
        }
        dispatch(&mut plan, a, &worker()).unwrap();
        assert_eq!(
            salvage_assignment(&mut plan, a, 2).unwrap(),
            Salvage::Exhausted {
                reclaims: 2,
                limit: 2
            }
        );
        assert!(matches!(
            plan.node(a).unwrap().status,
            TaskStatus::Failed { .. }
        ));
    }

    #[test]
    fn salvaging_an_actor_takes_only_its_own_in_flight_work() {
        let mut plan = plan(PlanMode::Light);
        let seeded = seed(&mut plan, &lead(), "k1", &three()).unwrap();
        dispatch(&mut plan, seeded.nodes[0], &worker()).unwrap();
        dispatch(&mut plan, seeded.nodes[1], &ActorId::new("other")).unwrap();
        let salvaged = salvage_actor(&mut plan, &worker(), 3);
        assert_eq!(salvaged.len(), 1);
        assert_eq!(salvaged[0].0, seeded.nodes[0]);
        assert!(plan.node(seeded.nodes[1]).unwrap().assignee().is_some());
    }

    #[test]
    fn only_the_owner_may_abandon() {
        let mut plan = plan(PlanMode::Light);
        let seeded = seed(&mut plan, &lead(), "k1", &three()).unwrap();
        let a = seeded.nodes[0];
        dispatch(&mut plan, a, &worker()).unwrap();
        assert!(matches!(
            abandon_node(&mut plan, a, &worker(), "moot"),
            Err(PlanError::NotOwner { .. })
        ));
        abandon_node(&mut plan, a, &lead(), "moot").unwrap();
    }

    #[test]
    fn a_cycle_is_refused_before_it_is_written() {
        let mut plan = plan(PlanMode::Light);
        let seeded = seed(&mut plan, &lead(), "k1", &three()).unwrap();
        let (a, c) = (seeded.nodes[0], seeded.nodes[2]);
        // `c` already waits on `a`. An edge making `a` wait on `c` would close it.
        let err = ensure_no_cycle(&plan, a, c).unwrap_err();
        assert_eq!(
            err,
            PlanError::WouldCycle {
                dependent: a,
                dependency: c
            }
        );
    }

    #[test]
    fn a_node_cannot_wait_on_itself() {
        let mut plan = plan(PlanMode::Light);
        let seeded = seed(&mut plan, &lead(), "k1", &three()).unwrap();
        let a = seeded.nodes[0];
        assert!(ensure_no_cycle(&plan, a, a).is_err());
    }

    #[test]
    fn a_batch_that_loops_on_itself_is_refused_before_anything_is_created() {
        let mut plan = plan(PlanMode::Light);
        let specs = vec![
            NodeSpec::task("a", "").after(1),
            NodeSpec::task("b", "").after(0),
        ];
        assert!(matches!(
            seed(&mut plan, &lead(), "k1", &specs),
            Err(PlanError::BatchCycle { .. })
        ));
        assert!(plan.is_empty());
    }

    #[test]
    fn a_batch_task_cannot_depend_on_itself() {
        let mut plan = plan(PlanMode::Light);
        let specs = vec![NodeSpec::task("a", "").after(0)];
        assert_eq!(
            seed(&mut plan, &lead(), "k1", &specs),
            Err(PlanError::BatchCycle { index: 0 })
        );
    }

    #[test]
    fn a_long_batch_chain_validates_without_recursing() {
        let specs: Vec<NodeSpec> = (0..5_000)
            .map(|i| {
                let spec = NodeSpec::task(format!("t{i}"), "");
                if i == 0 {
                    spec
                } else {
                    spec.after(i - 1)
                }
            })
            .collect();
        assert!(validate_batch(&specs).is_ok());
    }

    #[test]
    fn a_diamond_inside_a_batch_is_not_mistaken_for_a_loop() {
        let specs = vec![
            NodeSpec::task("root", ""),
            NodeSpec::task("left", "").after(0),
            NodeSpec::task("right", "").after(0),
            NodeSpec::task("join", "").after(1).after(2),
        ];
        assert!(validate_batch(&specs).is_ok());
    }

    #[test]
    fn a_forward_reference_inside_a_batch_resolves() {
        let mut plan = plan(PlanMode::Light);
        let specs = vec![
            NodeSpec::task("first", "").after(1),
            NodeSpec::task("second", ""),
        ];
        let seeded = seed(&mut plan, &lead(), "k1", &specs).unwrap();
        assert_eq!(
            plan.node(seeded.nodes[0]).unwrap().upstream,
            vec![seeded.nodes[1]]
        );
    }
}
