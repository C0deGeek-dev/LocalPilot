//! Carrying finished work across a replan.
//!
//! Replanning changes future intent. It is not a reset: the commits behind
//! completed steps exist, and a new plan that quietly dropped them would ask for
//! work that is already in the repository. So a replan drafts against the
//! current brief and is then reconciled with the plan it replaces, moving
//! `done`, `commit` and `attempts` onto their counterparts.
//!
//! Reconciliation is deterministic or it is a conflict. Where a completed step
//! has exactly one counterpart, the evidence moves; where it has none, several,
//! or where the new plan would credit that finished commit with more than it was
//! ever checked against, nothing is written and the ambiguity is named for a
//! person to settle. The alternative — guessing — is how a plan comes to claim
//! that an old commit satisfies an acceptance criterion written after it.
//!
//! What the draft may say about finished work is deliberately narrow. Its
//! number, wording, commit, attempt count and verification all belong to the
//! past: a check chosen after the commit did not verify it, and a plan that said
//! otherwise would be a plan nobody can audit. Only `covers` may change, only
//! downwards, and only through the credit check.

use crate::progress::{Progress, Step};

/// A reconciliation that cannot be completed without a human decision.
///
/// Every variant names the step and the wording it is about; a conflict a person
/// cannot locate is a conflict they cannot settle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileConflict {
    /// Finished, committed work that the new plan does not contain.
    CompletedStepMissing {
        step: usize,
        description: String,
        commit: Option<String>,
    },
    /// The new plan repeats a completed step's wording, so which of them holds
    /// the evidence is not determined.
    AmbiguousMatch {
        description: String,
        candidates: Vec<usize>,
    },
    /// The old plan already repeated that wording, so the evidence cannot be
    /// attributed to one step even before the new plan is considered.
    IndistinctHistory {
        description: String,
        steps: Vec<usize>,
    },
    /// The new plan credits finished work with acceptance criteria it was never
    /// checked against.
    CreditNotVerifiable {
        step: usize,
        description: String,
        criteria: Vec<usize>,
        reason: CreditReason,
    },
    /// The new plan says finished work waits on work that is not done. History
    /// says otherwise, and one of the two is wrong.
    CompletedDependsOnPending { step: usize, depends_on: usize },
}

/// Why a credit cannot be carried forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreditReason {
    /// The old plan was bound to a different brief revision, so its criterion
    /// numbers and the new plan's are not the same criteria.
    BriefChanged,
    /// The old plan carried no binding at all, so what it was checked against is
    /// unknown rather than merely different.
    BriefUnknown,
    /// Same brief, but the new plan claims criteria the completed step did not.
    Widened { was: Vec<usize> },
}

impl std::fmt::Display for ReconcileConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CompletedStepMissing {
                step, description, ..
            } => write!(
                f,
                "step {step} ({description}) is done, but the new plan does not contain it"
            ),
            Self::AmbiguousMatch {
                description,
                candidates,
            } => write!(
                f,
                "the new plan has {} steps worded '{description}' (steps {}), so the completed work cannot be matched to one",
                candidates.len(),
                join(candidates)
            ),
            Self::IndistinctHistory { description, steps } => write!(
                f,
                "the previous plan has more than one completed step worded '{description}' (steps {}), so its evidence cannot be attributed",
                join(steps)
            ),
            Self::CreditNotVerifiable {
                step,
                description,
                criteria,
                reason,
            } => write!(
                f,
                "step {step} ({description}) is already done, and the new plan credits it with {} - {reason}",
                join_criteria(criteria)
            ),
            Self::CompletedDependsOnPending { step, depends_on } => write!(
                f,
                "step {step} is done but the new plan says it depends on step {depends_on}, which is not"
            ),
        }
    }
}

impl std::fmt::Display for CreditReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BriefChanged => write!(
                f,
                "the brief has changed since that work was done, so those numbers are not the criteria it was checked against"
            ),
            Self::BriefUnknown => write!(
                f,
                "the previous plan names no brief revision, so what that work was checked against is unknown"
            ),
            Self::Widened { was } => {
                if was.is_empty() {
                    write!(f, "it previously covered nothing")
                } else {
                    write!(f, "it previously covered only {}", join_criteria(was))
                }
            }
        }
    }
}

fn join(numbers: &[usize]) -> String {
    numbers
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn join_criteria(numbers: &[usize]) -> String {
    numbers
        .iter()
        .map(|number| format!("AC{number}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A step's identity across a redraft.
///
/// The replan prompt requires completed steps to be reproduced verbatim, so this
/// only has to survive incidental respacing and capitalisation. Anything looser
/// would match *different* work and move a commit onto it, which is the failure
/// this whole module exists to prevent.
fn identity(description: &str) -> String {
    description
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Move completed evidence from `old` into a plan built around it.
///
/// `brief_revision` is the revision `new` will be bound to; it decides whether
/// the two plans' criterion numbers mean the same criteria at all.
///
/// Finished work is not the draft's to restate. A completed step keeps its
/// original number and its original wording — the number its commit is recorded
/// against, and the words it was done under — and the reconciled plan places
/// those steps first, in their original order, with the draft's remaining steps
/// numbered after them. Only future work is renumbered, which is the whole
/// point: a plan that renumbered history would file a commit under a step that
/// never produced it.
///
/// The reconciled plan keeps the old plan's branch. Replanning does not move the
/// work to a different branch, and the commits being carried are on this one.
///
/// # Errors
/// Returns every conflict found, and writes nothing.
pub fn reconcile(
    old: &Progress,
    new: &Progress,
    brief_revision: &str,
) -> Result<Progress, Vec<ReconcileConflict>> {
    let mut conflicts = Vec::new();

    let unverifiable = match old.brief_binding.as_deref() {
        Some(binding) if binding == brief_revision => None,
        Some(_) => Some(CreditReason::BriefChanged),
        None => Some(CreditReason::BriefUnknown),
    };

    // Which draft step each completed step is carried into, by draft index.
    let mut carried: Vec<(usize, &Step)> = Vec::new();

    for completed in old.steps.iter().filter(|step| step.done) {
        let key = identity(&completed.description);

        let twins: Vec<usize> = old
            .steps
            .iter()
            .filter(|step| step.done && identity(&step.description) == key)
            .map(|step| step.number)
            .collect();
        if twins.len() > 1 {
            // Reported once per twin, which is right: each one is a step whose
            // evidence went nowhere.
            conflicts.push(ReconcileConflict::IndistinctHistory {
                description: completed.description.clone(),
                steps: twins,
            });
            continue;
        }

        let candidates: Vec<usize> = new
            .steps
            .iter()
            .enumerate()
            .filter(|(_, step)| identity(&step.description) == key)
            .map(|(index, _)| index)
            .collect();

        match candidates.as_slice() {
            [] => conflicts.push(ReconcileConflict::CompletedStepMissing {
                step: completed.number,
                description: completed.description.clone(),
                commit: completed.commit.clone(),
            }),
            [index] => {
                let target = &new.steps[*index];
                if let Some(problem) = credit_problem(completed, target, unverifiable.as_ref()) {
                    conflicts.push(ReconcileConflict::CreditNotVerifiable {
                        step: completed.number,
                        description: completed.description.clone(),
                        criteria: target.covers.clone().unwrap_or_default(),
                        reason: problem,
                    });
                    continue;
                }
                carried.push((*index, completed));
            }
            several => conflicts.push(ReconcileConflict::AmbiguousMatch {
                description: completed.description.clone(),
                candidates: several
                    .iter()
                    .map(|index| new.steps[*index].number)
                    .collect(),
            }),
        }
    }

    if !conflicts.is_empty() {
        return Err(conflicts);
    }

    let reconciled = assemble(old, new, &carried, &mut conflicts);

    if conflicts.is_empty() {
        Ok(reconciled)
    } else {
        Err(conflicts)
    }
}

/// Build the reconciled plan: history first at its own numbers, then the
/// draft's remaining steps numbered after it.
fn assemble(
    old: &Progress,
    new: &Progress,
    carried: &[(usize, &Step)],
    conflicts: &mut Vec<ReconcileConflict>,
) -> Progress {
    // Draft numbering is the draft's own; the reconciled plan's is not. Every
    // `depends` has to move with it, or a dependency would silently come to
    // mean a different step.
    let mut final_number: Vec<Option<usize>> = vec![None; new.steps.len()];
    for (index, completed) in carried {
        final_number[*index] = Some(completed.number);
    }
    let highest = carried
        .iter()
        .map(|(_, completed)| completed.number)
        .max()
        .unwrap_or(0);
    let mut next = highest + 1;
    for slot in &mut final_number {
        if slot.is_none() {
            *slot = Some(next);
            next += 1;
        }
    }

    // A dependency the draft invented is left exactly as written rather than
    // dropped: approval reports it as unknown, and a silently removed
    // dependency is a defect nobody ever sees.
    let renumber_draft = |numbers: &Option<Vec<usize>>| -> Option<Vec<usize>> {
        numbers.as_ref().map(|numbers| {
            numbers
                .iter()
                .map(|number| {
                    new.steps
                        .iter()
                        .position(|step| step.number == *number)
                        .and_then(|index| final_number[index])
                        .unwrap_or(*number)
                })
                .collect()
        })
    };

    // A completed step's own order, re-expressed in the new numbering. An old
    // step it named survives either as carried history (same number) or as
    // redrafted work found by identity; a reference to work the new plan does
    // not contain at all is left out, because the reconciled plan has no way to
    // name it and a number pointing at whatever now occupies that slot would be
    // a different claim. Leaving it out narrows what the step asserts, which is
    // the same direction `covers` is allowed to move.
    let renumber_old = |numbers: &Option<Vec<usize>>| -> Option<Vec<usize>> {
        numbers.as_ref().map(|numbers| {
            numbers
                .iter()
                .filter_map(|number| {
                    let referenced = old.steps.iter().find(|step| step.number == *number)?;
                    if referenced.done {
                        return carried
                            .iter()
                            .any(|(_, carried)| carried.number == referenced.number)
                            .then_some(referenced.number);
                    }
                    let key = identity(&referenced.description);
                    new.steps
                        .iter()
                        .position(|step| identity(&step.description) == key)
                        .and_then(|index| final_number[index])
                })
                .collect()
        })
    };

    let mut steps: Vec<Step> = Vec::with_capacity(new.steps.len());

    // History first, in the order it happened, carrying its own identity. The
    // number a commit is recorded against, the wording it was done under, and
    // the check that verified it are facts about the past; a draft written
    // afterwards does not get to restate any of them. `covers` is the one field
    // the draft may change, and only downwards — the credit check above has
    // already refused any widening, and narrowing asserts less rather than more
    // (a criterion that loses its owner is then reported at approval).
    for completed in old.steps.iter().filter(|step| step.done) {
        let Some((index, _)) = carried
            .iter()
            .find(|(_, carried)| carried.number == completed.number)
        else {
            continue;
        };
        let source = &new.steps[*index];
        steps.push(Step {
            number: completed.number,
            description: completed.description.clone(),
            done: true,
            commit: completed.commit.clone(),
            attempts: completed.attempts,
            sessions: completed.sessions.clone(),
            covers: source.covers.clone(),
            verify: completed.verify.clone(),
            depends: renumber_old(&completed.depends),
        });
    }

    // Then the work still to do, in the order the draft put it.
    for (index, step) in new.steps.iter().enumerate() {
        if carried.iter().any(|(carried, _)| *carried == index) {
            continue;
        }
        let mut step = step.clone();
        step.depends = renumber_draft(&step.depends);
        step.number = final_number[index].unwrap_or(step.number);
        steps.push(step);
    }

    let reconciled = Progress {
        name: new.name.clone(),
        branch: old.branch.clone(),
        brief_binding: new.brief_binding.clone(),
        steps,
    };
    check_order(&reconciled, conflicts);
    reconciled
}

/// Whether the new plan credits finished work with more than it was checked
/// against. Claiming less is always allowed: a replan may decide a step no
/// longer owns a criterion, and nothing is asserted by that.
fn credit_problem(
    completed: &Step,
    target: &Step,
    unverifiable: Option<&CreditReason>,
) -> Option<CreditReason> {
    let claimed = target.covers.as_deref().unwrap_or_default();
    if claimed.is_empty() {
        return None;
    }
    if let Some(reason) = unverifiable {
        return Some(reason.clone());
    }
    let was = completed.covers.clone().unwrap_or_default();
    if claimed.iter().all(|criterion| was.contains(criterion)) {
        None
    } else {
        Some(CreditReason::Widened { was })
    }
}

fn check_order(reconciled: &Progress, conflicts: &mut Vec<ReconcileConflict>) {
    let pending: Vec<usize> = reconciled
        .steps
        .iter()
        .filter(|step| !step.done)
        .map(|step| step.number)
        .collect();
    for step in reconciled.steps.iter().filter(|step| step.done) {
        let Some(depends) = &step.depends else {
            continue;
        };
        for dependency in depends {
            if pending.contains(dependency) {
                conflicts.push(ReconcileConflict::CompletedDependsOnPending {
                    step: step.number,
                    depends_on: *dependency,
                });
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const REVISION: &str = "sha256-v1:abc";

    fn plan(branch: &str, binding: Option<&str>, steps: &str) -> Progress {
        let bound = binding.map_or(String::new(), |value| format!("Brief: {value}\n"));
        Progress::parse(&format!(
            "# Progress: thing\nBranch: {branch}\n{bound}\n## Steps\n\n{steps}"
        ))
        .unwrap()
    }

    #[test]
    fn finished_work_moves_onto_its_counterpart_and_keeps_the_branch() {
        let old = plan(
            "feature/old",
            Some(REVISION),
            "- [x] 1. Write the parser\n  - commit: abc1234\n  - attempts: 2\n  - covers: AC1\n  \
- verify: cargo test parser\n  - depends: none\n- [ ] 2. Write the gate\n  - covers: AC2\n  \
- verify: cargo test gate\n  - depends: 1\n",
        );
        let new = plan(
            "feature/new",
            None,
            "- [ ] 1. Write the parser\n  - covers: AC1\n  - verify: cargo test parser\n  \
- depends: none\n- [ ] 2. Write the reporter\n  - covers: AC2\n  - verify: cargo test reporter\n  \
- depends: 1\n",
        );

        let reconciled = reconcile(&old, &new, REVISION).unwrap();

        assert!(reconciled.steps[0].done);
        assert_eq!(reconciled.steps[0].commit.as_deref(), Some("abc1234"));
        assert_eq!(reconciled.steps[0].attempts, 2);
        assert!(!reconciled.steps[1].done);
        // Replanning does not move the work to another branch; the carried
        // commits are on this one.
        assert_eq!(reconciled.branch, "feature/old");
    }

    #[test]
    fn a_completed_step_keeps_its_number_when_the_draft_moves_it() {
        // A commit is recorded against a step number. A reconciliation that let
        // the draft's position decide that number would file finished work
        // under a step that never produced it.
        let old = plan(
            "feature/old",
            Some(REVISION),
            "- [ ] 1. Design the format\n  - covers: none\n  - verify: cargo test\n  \
- depends: none\n- [x] 2. Write the parser\n  - commit: abc1234\n  - covers: none\n  \
- verify: cargo test parser\n  - depends: 1\n",
        );
        let new = plan(
            "feature/old",
            None,
            "- [ ] 1. Write the parser\n  - covers: none\n  - verify: cargo test parser\n  \
- depends: none\n- [ ] 2. Write the reporter\n  - covers: none\n  - verify: cargo test\n  \
- depends: 1\n",
        );

        let reconciled = reconcile(&old, &new, REVISION).unwrap();

        assert_eq!(reconciled.steps[0].number, 2, "history keeps its number");
        assert!(reconciled.steps[0].done);
        assert_eq!(reconciled.steps[0].commit.as_deref(), Some("abc1234"));
        // Only future work is renumbered, and it is numbered after the history
        // it follows.
        assert_eq!(reconciled.steps[1].number, 3);
        assert_eq!(reconciled.steps[1].description, "Write the reporter");
        assert!(!reconciled.steps[1].done);
    }

    #[test]
    fn a_completed_step_keeps_the_wording_its_commit_was_made_under() {
        // Matching tolerates respacing so a redraft can be recognised; adopting
        // the redraft's wording would rewrite the record of what was done.
        let old = plan(
            "feature/old",
            Some(REVISION),
            "- [x] 1. Write the parser\n  - commit: abc1234\n  - covers: none\n",
        );
        let new = plan(
            "feature/old",
            None,
            "- [ ] 1. write   the PARSER\n  - covers: none\n  - verify: cargo test\n  \
- depends: none\n",
        );

        let reconciled = reconcile(&old, &new, REVISION).unwrap();

        assert_eq!(reconciled.steps[0].description, "Write the parser");
        // Nor its verification. The old plan stated none, so none is what the
        // reconciled plan states: a check chosen after the commit did not verify
        // it, and saying it did would make the record unauditable.
        assert_eq!(reconciled.steps[0].verify, None);
    }

    #[test]
    fn dependencies_are_remapped_from_draft_numbering_to_final_numbering() {
        let old = plan(
            "feature/old",
            Some(REVISION),
            "- [ ] 1. Design the format\n- [x] 2. Write the parser\n  - commit: abc1234\n  \
- covers: none\n",
        );
        let new = plan(
            "feature/old",
            None,
            "- [ ] 1. Write the parser\n  - covers: none\n  - verify: cargo test\n  \
- depends: none\n- [ ] 2. Write the gate\n  - covers: none\n  - verify: cargo test\n  \
- depends: 1\n- [ ] 3. Document it\n  - covers: none\n  - verify: none - prose only\n  \
- depends: 2\n",
        );

        let reconciled = reconcile(&old, &new, REVISION).unwrap();

        // Draft 1 became 2 (the completed step), draft 2 became 3, draft 3
        // became 4, and every dependency moved with them.
        let numbers: Vec<usize> = reconciled.steps.iter().map(|step| step.number).collect();
        assert_eq!(numbers, vec![2, 3, 4]);
        assert_eq!(reconciled.steps[1].depends, Some(vec![2]));
        assert_eq!(reconciled.steps[2].depends, Some(vec![3]));
    }

    #[test]
    fn a_gap_left_by_dropped_work_is_kept_rather_than_closed() {
        // Steps 1 and 3 were done and 2 was dropped. Closing the gap would move
        // step 3's commit to step 2.
        let old = plan(
            "feature/old",
            Some(REVISION),
            "- [x] 1. Write the parser\n  - commit: abc1234\n  - covers: none\n\
- [ ] 2. Write the gate\n- [x] 3. Write the reporter\n  - commit: def5678\n  - covers: none\n",
        );
        let new = plan(
            "feature/old",
            None,
            "- [ ] 1. Write the parser\n  - covers: none\n  - verify: cargo test\n  \
- depends: none\n- [ ] 2. Write the reporter\n  - covers: none\n  - verify: cargo test\n  \
- depends: 1\n- [ ] 3. Document it\n  - covers: none\n  - verify: none - prose only\n  \
- depends: 2\n",
        );

        let reconciled = reconcile(&old, &new, REVISION).unwrap();

        let numbers: Vec<usize> = reconciled.steps.iter().map(|step| step.number).collect();
        assert_eq!(numbers, vec![1, 3, 4]);
        assert_eq!(reconciled.steps[1].commit.as_deref(), Some("def5678"));
    }

    #[test]
    fn work_that_is_done_but_absent_from_the_new_plan_is_a_conflict() {
        let old = plan(
            "feature/old",
            Some(REVISION),
            "- [x] 1. Write the parser\n  - commit: abc1234\n",
        );
        let new = plan(
            "feature/old",
            None,
            "- [ ] 1. Write the reporter\n  - covers: none\n  - verify: cargo test\n  \
- depends: none\n",
        );

        let conflicts = reconcile(&old, &new, REVISION).unwrap_err();
        assert_eq!(
            conflicts,
            vec![ReconcileConflict::CompletedStepMissing {
                step: 1,
                description: "Write the parser".to_string(),
                commit: Some("abc1234".to_string()),
            }]
        );
    }

    #[test]
    fn a_commit_is_not_credited_with_criteria_from_a_brief_it_never_saw() {
        // The whole point of replanning is that the brief moved. Carrying a
        // criterion number across that move would assert something nobody
        // checked.
        let old = plan(
            "feature/old",
            Some("sha256-v1:older"),
            "- [x] 1. Write the parser\n  - commit: abc1234\n  - covers: AC1\n",
        );
        let new = plan(
            "feature/old",
            None,
            "- [ ] 1. Write the parser\n  - covers: AC1\n  - verify: cargo test\n  \
- depends: none\n",
        );

        let conflicts = reconcile(&old, &new, REVISION).unwrap_err();
        assert!(
            matches!(
                conflicts.as_slice(),
                [ReconcileConflict::CreditNotVerifiable {
                    step: 1,
                    reason: CreditReason::BriefChanged,
                    ..
                }]
            ),
            "{conflicts:?}"
        );
    }

    #[test]
    fn an_unbound_previous_plan_is_unknown_rather_than_merely_different() {
        let old = plan("feature/old", None, "- [x] 1. Write the parser\n");
        let new = plan(
            "feature/old",
            None,
            "- [ ] 1. Write the parser\n  - covers: AC1\n  - verify: cargo test\n  \
- depends: none\n",
        );

        let conflicts = reconcile(&old, &new, REVISION).unwrap_err();
        assert!(
            matches!(
                conflicts.as_slice(),
                [ReconcileConflict::CreditNotVerifiable {
                    reason: CreditReason::BriefUnknown,
                    ..
                }]
            ),
            "{conflicts:?}"
        );
    }

    #[test]
    fn a_replan_may_narrow_what_finished_work_covers_but_not_widen_it() {
        // Same brief, so the numbers are comparable. Claiming less than before
        // asserts nothing and is allowed; claiming more is the conflict.
        let old = plan(
            "feature/old",
            Some(REVISION),
            "- [x] 1. Write the parser\n  - commit: abc1234\n  - covers: AC1, AC2\n\
- [x] 2. Write the gate\n  - commit: def5678\n  - covers: AC1\n",
        );
        let new = plan(
            "feature/old",
            None,
            "- [ ] 1. Write the parser\n  - covers: AC1\n  - verify: cargo test\n  \
- depends: none\n- [ ] 2. Write the gate\n  - covers: AC1, AC2\n  - verify: cargo test\n  \
- depends: 1\n",
        );

        let conflicts = reconcile(&old, &new, REVISION).unwrap_err();
        assert_eq!(
            conflicts,
            vec![ReconcileConflict::CreditNotVerifiable {
                step: 2,
                description: "Write the gate".to_string(),
                criteria: vec![1, 2],
                reason: CreditReason::Widened { was: vec![1] },
            }]
        );
    }

    #[test]
    fn repeated_wording_is_named_rather_than_guessed() {
        let old = plan(
            "feature/old",
            Some(REVISION),
            "- [x] 1. Write the parser\n  - commit: abc1234\n",
        );
        let new = plan(
            "feature/old",
            None,
            "- [ ] 1. Write the parser\n  - covers: none\n  - verify: cargo test\n  \
- depends: none\n- [ ] 2. write   the PARSER\n  - covers: none\n  - verify: cargo test\n  \
- depends: 1\n",
        );

        let conflicts = reconcile(&old, &new, REVISION).unwrap_err();
        assert_eq!(
            conflicts,
            vec![ReconcileConflict::AmbiguousMatch {
                description: "Write the parser".to_string(),
                candidates: vec![1, 2],
            }]
        );
    }

    #[test]
    fn history_that_repeats_itself_cannot_be_attributed() {
        let old = plan(
            "feature/old",
            Some(REVISION),
            "- [x] 1. Write the parser\n  - commit: abc1234\n\
- [x] 2. Write the parser\n  - commit: def5678\n",
        );
        let new = plan(
            "feature/old",
            None,
            "- [ ] 1. Write the parser\n  - covers: none\n  - verify: cargo test\n  \
- depends: none\n",
        );

        let conflicts = reconcile(&old, &new, REVISION).unwrap_err();
        assert_eq!(conflicts.len(), 2, "{conflicts:?}");
        assert!(conflicts.iter().all(|conflict| matches!(
            conflict,
            ReconcileConflict::IndistinctHistory { steps, .. } if steps == &vec![1, 2]
        )));
    }

    #[test]
    fn finished_work_may_not_be_made_to_wait_on_unfinished_work() {
        // The order comes from the old plan, where step 2 was finished before
        // step 1 was. Carrying it forward is what surfaces the contradiction.
        let old = plan(
            "feature/old",
            Some(REVISION),
            "- [ ] 1. Design the format
- [x] 2. Write the parser
  - commit: abc1234
  - covers: none
  - depends: 1
",
        );
        let new = plan(
            "feature/old",
            None,
            "- [ ] 1. Write the parser
  - covers: none
  - verify: cargo test
  - depends: none
- [ ] 2. Design the format
  - covers: none
  - verify: cargo test
  - depends: 1
",
        );

        // The finished step keeps number 2; the work it waited on is redrafted
        // and becomes 3, and the recorded dependency is remapped to it. Done
        // work waiting on work that is not done is still a contradiction.
        let conflicts = reconcile(&old, &new, REVISION).unwrap_err();
        assert_eq!(
            conflicts,
            vec![ReconcileConflict::CompletedDependsOnPending {
                step: 2,
                depends_on: 3,
            }]
        );
    }

    #[test]
    fn a_completed_step_keeps_the_order_it_was_done_in() {
        // Between two finished steps the recorded order survives untouched,
        // because both keep their original numbers.
        let old = plan(
            "feature/old",
            Some(REVISION),
            "- [x] 1. Write the parser
  - commit: abc1234
  - covers: none
  - depends: none
- [x] 2. Write the gate
  - commit: def5678
  - covers: none
  - depends: 1
",
        );
        let new = plan(
            "feature/old",
            None,
            "- [ ] 1. Write the gate
  - covers: none
  - verify: cargo test
  - depends: none
- [ ] 2. Write the parser
  - covers: none
  - verify: cargo test
  - depends: 1
",
        );

        let reconciled = reconcile(&old, &new, REVISION).unwrap();

        assert_eq!(reconciled.steps[0].number, 1);
        assert_eq!(reconciled.steps[0].depends, Some(Vec::new()));
        assert_eq!(reconciled.steps[1].number, 2);
        // Not the draft's `depends: 1`, which happened to mean the gate there.
        assert_eq!(reconciled.steps[1].depends, Some(vec![1]));
    }

    #[test]
    fn a_dependency_on_work_the_new_plan_drops_is_left_out_rather_than_left_dangling() {
        // The reconciled plan cannot name work it does not contain, and a
        // number pointing at whatever now occupies that slot would assert
        // something else entirely.
        let old = plan(
            "feature/old",
            Some(REVISION),
            "- [ ] 1. Survey the options
- [x] 2. Write the parser
  - commit: abc1234
  - covers: none
  - depends: 1
",
        );
        let new = plan(
            "feature/old",
            None,
            "- [ ] 1. Write the parser
  - covers: none
  - verify: cargo test
  - depends: none
",
        );

        let reconciled = reconcile(&old, &new, REVISION).unwrap();

        assert_eq!(reconciled.steps[0].number, 2);
        assert_eq!(reconciled.steps[0].depends, Some(Vec::new()));
    }

    #[test]
    fn nothing_is_carried_when_anything_conflicts() {
        // The caller gets conflicts or a plan, never a half-reconciled one: a
        // plan with some evidence moved and some not is worse than either.
        let old = plan(
            "feature/old",
            Some(REVISION),
            "- [x] 1. Write the parser\n  - commit: abc1234\n  - covers: none\n\
- [x] 2. Write the gate\n  - commit: def5678\n  - covers: none\n",
        );
        let new = plan(
            "feature/old",
            None,
            "- [ ] 1. Write the parser\n  - covers: none\n  - verify: cargo test\n  \
- depends: none\n",
        );

        assert!(reconcile(&old, &new, REVISION).is_err());
    }
}
